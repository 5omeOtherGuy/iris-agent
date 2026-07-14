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

use crate::config::{OpenAiCompatibleSettings, Settings};
use crate::errors::UsageError;
use crate::wayland::CacheProfile;

/// The providers Iris supports today. Parsing keeps the "unsupported provider"
/// error close to its only authority, so `build_provider` no longer matches on
/// raw strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderId {
    OpenAiCodex,
    OpenAi,
    Anthropic,
    Antigravity,
    OpenAiCompatible,
}

impl ProviderId {
    /// Provider used when the settings file selects none. Stays `openai-codex`
    /// for backward compatibility.
    pub(crate) const DEFAULT: ProviderId = ProviderId::OpenAiCodex;

    /// Every supported provider, in display/registry order. Used by the model
    /// catalog and the `/login` provider list so a new provider is added in one
    /// place.
    pub(crate) const ALL: [ProviderId; 5] = [
        ProviderId::OpenAiCodex,
        ProviderId::OpenAi,
        ProviderId::Anthropic,
        ProviderId::Antigravity,
        ProviderId::OpenAiCompatible,
    ];

    /// Human-facing provider name for selectors and status lines. Today this is
    /// just the wire id; kept as a separate accessor so a friendlier label can
    /// be added without touching call sites.
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            ProviderId::OpenAiCodex => "OpenAI Codex",
            ProviderId::OpenAi => "OpenAI API",
            ProviderId::Anthropic => "Anthropic",
            ProviderId::Antigravity => "Antigravity",
            ProviderId::OpenAiCompatible => "OpenAI-compatible",
        }
    }

    /// Parse a provider id string. The error mirrors the message and exit-code
    /// classification (`UsageError`) `build_provider` used to emit, so an
    /// unsupported value still fails loudly with the usage exit code.
    pub(crate) fn parse(value: &str) -> Result<ProviderId> {
        match value.trim() {
            "openai-codex" => Ok(ProviderId::OpenAiCodex),
            "openai" => Ok(ProviderId::OpenAi),
            "anthropic" => Ok(ProviderId::Anthropic),
            "antigravity" => Ok(ProviderId::Antigravity),
            "openai-compatible" => Ok(ProviderId::OpenAiCompatible),
            other => Err(UsageError::new(format!(
                "unsupported provider '{other}'; supported: openai-codex, openai, anthropic, antigravity, openai-compatible"
            ))
            .into()),
        }
    }

    /// The wire id, used for display and the recorded `modelSelection` event.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ProviderId::OpenAiCodex => "openai-codex",
            ProviderId::OpenAi => "openai",
            ProviderId::Anthropic => "anthropic",
            ProviderId::Antigravity => "antigravity",
            ProviderId::OpenAiCompatible => "openai-compatible",
        }
    }

    /// Built-in default model for this provider. Inherited placeholders already
    /// present in the per-adapter constants; centralized here so selection owns
    /// the model default.
    pub(crate) fn default_model(self) -> &'static str {
        match self {
            ProviderId::OpenAiCodex => "gpt-5.6-sol",
            ProviderId::OpenAi => "gpt-4.1",
            ProviderId::Anthropic => "claude-sonnet-4-6",
            ProviderId::Antigravity => "gemini-3.5-flash",
            ProviderId::OpenAiCompatible => "llama3.1",
        }
    }

    /// Built-in default base URL for this provider's API endpoint.
    fn default_base_url(self) -> &'static str {
        match self {
            ProviderId::OpenAiCodex => "https://chatgpt.com/backend-api",
            ProviderId::OpenAi => "https://api.openai.com/v1",
            ProviderId::Anthropic => "https://api.anthropic.com",
            ProviderId::Antigravity => "https://daily-cloudcode-pa.googleapis.com",
            ProviderId::OpenAiCompatible => "http://localhost:11434/v1",
        }
    }

    /// Provider-specific base-url env override, if any. Only Codex exposes one
    /// today (`IRIS_CODEX_BASE_URL`); the others have no env override.
    fn base_url_env(self) -> Option<&'static str> {
        match self {
            ProviderId::OpenAiCodex => Some("IRIS_CODEX_BASE_URL"),
            ProviderId::OpenAi
            | ProviderId::Anthropic
            | ProviderId::Antigravity
            | ProviderId::OpenAiCompatible => None,
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
    Max,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CodexTransport {
    #[default]
    Auto,
    Sse,
}

impl CodexTransport {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "auto" => Ok(Self::Auto),
            "sse" => Ok(Self::Sse),
            other => Err(UsageError::new(format!(
                "unsupported codexTransport '{other}'; supported: auto, sse"
            ))
            .into()),
        }
    }
}

impl ReasoningEffort {
    /// Default thinking/effort level (`medium`), matching pi-mono. Used when a
    /// picker needs a starting level and the session has no explicit preference.
    pub(crate) const DEFAULT: ReasoningEffort = ReasoningEffort::Medium;

    /// Every level, in increasing order. Used to round-trip parsing in tests.
    #[cfg(test)]
    pub(crate) const ALL: [ReasoningEffort; 7] = [
        ReasoningEffort::Off,
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
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
            "max" => Ok(ReasoningEffort::Max),
            other => Err(UsageError::new(format!(
                "unsupported reasoning level '{other}'; supported: off, minimal, low, medium, high, xhigh, max"
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
            ReasoningEffort::Max => "max",
        }
    }
}

/// Prompt-cache retention preference shared by provider adapters. `Short` (the
/// default) opts into the provider default ephemeral cache (Anthropic 5-minute
/// `cache_control` / OpenAI `prompt_cache_key`); `Long` opts into the provider's
/// longer-lived cache marker (Anthropic `ttl: "1h"` / OpenAI
/// `prompt_cache_retention: "24h"`); `None` disables every provider request
/// cache hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptCacheRetention {
    None,
    Short,
    Long,
}

impl PromptCacheRetention {
    /// Default cache retention: `Short`, matching minimalcc-pi's Claude
    /// subscription default and avoiding repeated uncached stable prefixes in
    /// multi-tool turns.
    pub(crate) const DEFAULT: PromptCacheRetention = PromptCacheRetention::Short;

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

/// Resolve the active selection to the provider-neutral prompt-cache profile
/// the Tier-2 fold scheduler consumes (issue #400, design §4.3). Provider
/// names stop here: wayland receives only [`CacheProfile`] fields. The table
/// is static, from provider documentation and measured pricing ratios:
///
/// | profile | cold_after | write premium | read rate | reports writes | min |
/// |---|---|---|---|---|---|
/// | Anthropic `short` | 6 min | 1.25 | 0.10 | yes | 1024 (2048 Haiku-class) |
/// | Anthropic `long` | 72 min | 2.0 | 0.10 | yes | 1024 (2048 Haiku-class) |
/// | OpenAI Responses lanes | 60 min | 1.0 | 0.10 | no | 1024 |
/// | unknown / caching off | none | 1.0 | 1.0 | no | 0 |
///
/// `cold_after` builds a margin over the documented TTL (5 min x 1.2; 60 min
/// x 1.2 for the 1h tier) so a racing refresh is never inferred cold. The
/// OpenAI in-memory cache documents 5-10 min typical eviction with a 1 h hard
/// maximum: 60 min is the guaranteed-cold bound, the probabilistic 12 min
/// option is recorded on the profile but not consumed yet. Unknown providers
/// (and `retention: none`, which disables every cache hint) degrade to the
/// safe default: cold-based triggers off, no minimum, break events still
/// valid.
pub(crate) fn cache_profile(selection: &ModelSelection) -> CacheProfile {
    use std::time::Duration;
    if !selection.cache_retention.caching_enabled() {
        return CacheProfile::default();
    }
    match selection.provider {
        ProviderId::Anthropic => {
            // Anthropic documents a larger minimum cacheable prefix for the
            // Haiku-class models (2048 tokens vs 1024).
            let min = if selection.model.to_ascii_lowercase().contains("haiku") {
                2048
            } else {
                1024
            };
            let (cold_after, write_premium) = match selection.cache_retention {
                // 5-minute tier x 1.2 margin; writes bill at 1.25x base.
                PromptCacheRetention::Short => (Duration::from_secs(6 * 60), 1.25),
                // 1-hour tier x 1.2 margin; writes bill at 2x base.
                PromptCacheRetention::Long => (Duration::from_secs(72 * 60), 2.0),
                PromptCacheRetention::None => unreachable!("caching_enabled checked above"),
            };
            CacheProfile {
                cold_after: Some(cold_after),
                probably_cold_after: None,
                write_premium,
                read_rate: 0.10,
                reports_writes: true,
                min_cacheable_tokens: min,
            }
        }
        ProviderId::OpenAiCodex | ProviderId::OpenAi => CacheProfile {
            // In-memory prompt cache: 5-10 min typical inactivity eviction,
            // hard max 1 h -- 60 min is the guaranteed-cold bound. Extended
            // (24 h) retention is unverified on the subscription backend;
            // until measured (#395 follow-up) the 1 h bound stands, and a
            // wrong inference costs one warm flush on a lane with no write
            // premium.
            cold_after: Some(Duration::from_secs(60 * 60)),
            probably_cold_after: Some(Duration::from_secs(12 * 60)),
            write_premium: 1.0,
            read_rate: 0.10,
            reports_writes: false,
            min_cacheable_tokens: 1024,
        },
        ProviderId::Antigravity | ProviderId::OpenAiCompatible => CacheProfile::default(),
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
    pub(crate) exclude_tools: Option<Vec<String>>,
    pub(crate) clear_tool_inputs: Option<bool>,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct OpenAiCompatibleConfig {
    pub(crate) context_window: Option<u64>,
    pub(crate) reasoning: bool,
    pub(crate) api_key_required: bool,
}

impl OpenAiCompatibleConfig {
    pub(crate) fn from_settings(settings: Option<&OpenAiCompatibleSettings>) -> Self {
        let Some(settings) = settings else {
            return Self::default();
        };
        Self {
            context_window: settings.context_window,
            reasoning: settings.reasoning.unwrap_or(false),
            api_key_required: settings.api_key_required.unwrap_or(false),
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
    pub(crate) cache_retention: PromptCacheRetention,
    pub(crate) codex_transport: CodexTransport,
    /// OpenAI Codex raw WebSocket/SSE read-idle policy. `None` disables idle
    /// detection without changing connect, send, cancellation, or total bounds.
    pub(crate) codex_stream_idle_timeout: Option<std::time::Duration>,
    /// Anthropic-only context-management opt-in; empty/default for other
    /// providers and when unconfigured.
    pub(crate) context_management: ContextManagement,
    /// Raw legacy advanced block retained so runtime provider switches can
    /// rebuild the provider-specific edit without losing `clearThinking`.
    pub(crate) legacy_context_management: ContextManagement,
    /// Provider-resolved local policy installed into Wayland. Native B is
    /// disabled here while semantic C may remain local when disjoint.
    pub(crate) tool_result_compaction: crate::config::ToolResultCompactionPolicy,
    /// Configured policy before provider resolution. Retained for `/model`
    /// switches, especially `auto` fallback.
    pub(crate) configured_tool_result_compaction: crate::config::ToolResultCompactionPolicy,
    /// Shared provider retry/backoff policy, resolved from settings with
    /// pi-mono-aligned defaults. Every provider adapter uses this single
    /// definition instead of its own retry constants.
    pub(crate) retry_policy: crate::mimir::retry::RetryPolicy,
    /// Generic OpenAI-compatible endpoint capability/display metadata.
    pub(crate) open_ai_compatible: OpenAiCompatibleConfig,
}

impl ModelSelection {
    pub(crate) fn resolve_compaction_worker(settings: &Settings, qualified: &str) -> Result<Self> {
        let (provider, model) = qualified
            .split_once('/')
            .ok_or_else(|| UsageError::new("compaction.worker.model must be provider/model"))?;
        let provider = ProviderId::parse(provider)?;
        let model = model.trim();
        if model.is_empty() {
            return Err(UsageError::new("compaction.worker.model must be provider/model").into());
        }
        let mut selection = Self::resolve(settings)?;
        selection.provider = provider;
        selection.model = model.to_string();
        selection.base_url = base_url_for(provider, settings.base_url.as_deref());
        selection.reasoning = None;
        selection.resolve_context_management_for_provider()?;
        Ok(selection)
    }

    /// Resolve the selection from settings, centralizing precedence:
    /// - provider: `settings.default_provider` -> `openai-codex`
    /// - model: `IRIS_MODEL` -> `settings.default_model` -> per-provider default
    /// - base_url: provider env (`IRIS_CODEX_BASE_URL` only today) ->
    ///   `settings.base_url` -> per-provider default
    /// - reasoning: `settings.default_reasoning` parsed for the resolved
    ///   provider/model -> else `None`
    /// - cache retention: `settings.prompt_cache_retention` -> `short`
    /// - context management: `settings.anthropic_context_management` -> empty
    ///
    /// `settings.base_url` is already global-only (the security invariant is
    /// enforced in `Settings::merged_with`), so resolve never re-derives it from
    /// untrusted project config.
    pub(crate) fn resolve(settings: &Settings) -> Result<ModelSelection> {
        let provider = match trimmed_non_empty(settings.default_provider.as_deref()) {
            Some(value) => ProviderId::parse(value)?,
            None => default_provider_from_env(),
        };
        let model = non_empty_env("IRIS_MODEL")
            .or_else(|| trimmed_non_empty(settings.default_model.as_deref()).map(str::to_string))
            .unwrap_or_else(|| provider.default_model().to_string());
        let base_url = base_url_for(provider, settings.base_url.as_deref());
        let reasoning = match trimmed_non_empty(settings.default_reasoning.as_deref()) {
            Some(value) => Some(crate::mimir::model_capabilities::parse_persisted_level(
                provider, &model, value,
            )?),
            None => None,
        };
        let cache_retention = match trimmed_non_empty(settings.prompt_cache_retention.as_deref()) {
            Some(value) => PromptCacheRetention::parse(value)?,
            None => PromptCacheRetention::DEFAULT,
        };
        let codex_transport = match trimmed_non_empty(settings.codex_transport.as_deref()) {
            Some(value) => CodexTransport::parse(value)?,
            None => CodexTransport::Auto,
        };
        let codex_stream_idle_timeout = settings.codex_stream_idle_timeout();
        let legacy_context_management = match &settings.anthropic_context_management {
            Some(value) => {
                serde_json::from_value::<ContextManagement>(value.clone()).map_err(|error| {
                    UsageError::new(format!(
                        "invalid anthropicContextManagement config: {error}"
                    ))
                })?
            }
            None => ContextManagement::default(),
        };
        legacy_context_management.validate_supported()?;
        let configured_tool_result_compaction = settings.tool_result_compaction()?;
        let retry_policy =
            crate::mimir::retry::RetryPolicy::from_settings(&settings.retry_settings());
        let open_ai_compatible =
            OpenAiCompatibleConfig::from_settings(settings.open_ai_compatible.as_ref());
        let mut selection = ModelSelection {
            provider,
            model,
            base_url,
            reasoning,
            cache_retention,
            codex_transport,
            codex_stream_idle_timeout,
            context_management: ContextManagement::default(),
            legacy_context_management,
            tool_result_compaction: configured_tool_result_compaction.clone(),
            configured_tool_result_compaction,
            retry_policy,
            open_ai_compatible,
        };
        selection.resolve_context_management_for_provider()?;
        Ok(selection)
    }

    /// Re-resolve local/native tool-result compaction for the current provider.
    /// Called at startup and immediately before a runtime provider switch.
    pub(crate) fn resolve_context_management_for_provider(&mut self) -> Result<()> {
        let mut context_management = self.legacy_context_management.clone();
        context_management.validate_supported()?;
        let mut local = self.configured_tool_result_compaction.clone();
        let configured = &local.tool_clearing;
        let structured_native = local.enabled
            && configured.enabled
            && match configured.backend {
                crate::config::ToolClearingBackend::AnthropicNative => {
                    if self.provider != ProviderId::Anthropic {
                        return Err(UsageError::new(format!(
                            "toolResultCompaction.toolClearing.backend=anthropicNative is not supported by provider {}; use backend=auto to fall back to local",
                            self.provider.as_str()
                        ))
                        .into());
                    }
                    true
                }
                crate::config::ToolClearingBackend::Auto => {
                    self.provider == ProviderId::Anthropic
                        && native_is_disjoint_from_semantic(&local)
                }
                crate::config::ToolClearingBackend::Local => false,
            };

        if context_management.clear_tool_uses.is_some() && self.provider != ProviderId::Anthropic {
            return Err(UsageError::new(format!(
                "anthropicContextManagement.clearToolUses is not supported by provider {}",
                self.provider.as_str()
            ))
            .into());
        }
        if structured_native {
            if !configured.include_failures {
                return Err(UsageError::new(
                    "toolResultCompaction.toolClearing.backend=anthropicNative cannot enforce includeFailures=false because Anthropic exposes no result-status filter; set includeFailures=true explicitly or use backend=local/auto",
                )
                .into());
            }
            if context_management.clear_tool_uses.is_some() {
                return Err(UsageError::new(
                    "toolResultCompaction.toolClearing and anthropicContextManagement.clearToolUses both configure Anthropic-native tool clearing; configure only one",
                )
                .into());
            }
            let native_excluded = native_excluded_tools(&local);
            let mut local_after_delegation = local.clone();
            local_after_delegation.tool_clearing.enabled = false;
            validate_native_disjoint(&local_after_delegation, &native_excluded)?;
            context_management.clear_tool_uses = Some(ClearToolUses {
                trigger_input_tokens: Some(local.trigger_tokens),
                keep_tool_uses: Some(configured.keep_recent_tool_uses),
                clear_at_least_input_tokens: Some(configured.clear_at_least_tokens),
                exclude_tools: Some(native_excluded),
                clear_tool_inputs: Some(configured.clear_tool_inputs),
            });
            local.tool_clearing.enabled = false;
        } else if let Some(native) = &context_management.clear_tool_uses {
            validate_native_disjoint(&local, native.exclude_tools.as_deref().unwrap_or_default())?;
        }
        self.context_management = context_management;
        self.tool_result_compaction = local;
        Ok(())
    }
}

const BUILT_IN_TOOL_NAMES: &[&str] = &[
    "read",
    "write",
    "edit",
    "bash",
    "grep",
    "find",
    "ls",
    "read_output",
    "recall",
];

fn local_candidate_tools(policy: &crate::config::ToolResultCompactionPolicy) -> Vec<String> {
    let mut candidates = Vec::new();
    if policy.enabled && policy.semantic_dedupe.enabled {
        candidates.extend(["read".to_string(), "ls".to_string()]);
    }
    let clearing = &policy.tool_clearing;
    if policy.enabled
        && clearing.enabled
        && matches!(
            clearing.backend,
            crate::config::ToolClearingBackend::Local | crate::config::ToolClearingBackend::Auto
        )
    {
        let base: Vec<&str> = match clearing.mode {
            crate::config::ToolClearingMode::Replayable => {
                vec!["read", "ls", "grep", "find"]
            }
            crate::config::ToolClearingMode::Selected => {
                clearing.eligible_tools.iter().map(String::as_str).collect()
            }
            crate::config::ToolClearingMode::AllRecoverable => BUILT_IN_TOOL_NAMES.to_vec(),
        };
        candidates.extend(
            base.into_iter()
                .filter(|name| {
                    !clearing
                        .excluded_tools
                        .iter()
                        .any(|excluded| excluded == name)
                        && (clearing.eligible_tools.is_empty()
                            || clearing.mode == crate::config::ToolClearingMode::Selected
                            || clearing
                                .eligible_tools
                                .iter()
                                .any(|eligible| eligible == name))
                })
                .map(str::to_owned),
        );
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn native_is_disjoint_from_semantic(policy: &crate::config::ToolResultCompactionPolicy) -> bool {
    policy.tool_clearing.include_failures
        && (!policy.semantic_dedupe.enabled
            || ["read", "ls"].iter().all(|name| {
                native_excluded_tools(policy)
                    .iter()
                    .any(|excluded| excluded == name)
            }))
}

fn native_excluded_tools(policy: &crate::config::ToolResultCompactionPolicy) -> Vec<String> {
    let clearing = &policy.tool_clearing;
    let mut excluded = clearing.excluded_tools.clone();
    let eligible: Vec<&str> = match clearing.mode {
        crate::config::ToolClearingMode::Replayable => vec!["read", "ls", "grep", "find"],
        crate::config::ToolClearingMode::Selected => {
            clearing.eligible_tools.iter().map(String::as_str).collect()
        }
        crate::config::ToolClearingMode::AllRecoverable => BUILT_IN_TOOL_NAMES.to_vec(),
    };
    for name in BUILT_IN_TOOL_NAMES {
        if !eligible.contains(name)
            || (!clearing.eligible_tools.is_empty()
                && !clearing
                    .eligible_tools
                    .iter()
                    .any(|eligible| eligible == name))
        {
            excluded.push((*name).to_string());
        }
    }
    excluded.sort();
    excluded.dedup();
    excluded
}

fn validate_native_disjoint(
    local: &crate::config::ToolResultCompactionPolicy,
    native_excluded: &[String],
) -> Result<()> {
    let overlap: Vec<String> = local_candidate_tools(local)
        .into_iter()
        .filter(|name| !native_excluded.iter().any(|excluded| excluded == name))
        .collect();
    if !overlap.is_empty() {
        let local_setting = if local.legacy_alias {
            "microcompaction (legacy alias for toolResultCompaction)"
        } else {
            "toolResultCompaction"
        };
        return Err(UsageError::new(format!(
            "local {local_setting} reducers overlap Anthropic-native clearToolUses for tools [{}]; add them to excludeTools or disable the overlapping local reducer",
            overlap.join(", ")
        ))
        .into());
    }
    Ok(())
}

fn default_provider_from_env() -> ProviderId {
    if non_empty_env("OPENAI_API_KEY").is_some() {
        return ProviderId::OpenAi;
    }
    if non_empty_env("ANTHROPIC_API_KEY").is_some() {
        return ProviderId::Anthropic;
    }
    if non_empty_env("OPENAI_COMPATIBLE_API_KEY").is_some()
        || non_empty_env("IRIS_OPENAI_COMPATIBLE_API_KEY").is_some()
    {
        return ProviderId::OpenAiCompatible;
    }
    ProviderId::DEFAULT
}

/// Resolve a base URL for a provider with precedence `env > settings > default`.
/// Pass `settings_base_url = None` to ignore the settings value (used on a
/// runtime `/model` provider switch: the configured `base_url` binds to the
/// originally selected provider and must not silently redirect a different one).
pub(crate) fn base_url_for(provider: ProviderId, settings_base_url: Option<&str>) -> String {
    provider
        .base_url_env()
        .and_then(non_empty_env)
        .or_else(|| settings_base_url_for(provider, settings_base_url))
        .unwrap_or_else(|| provider.default_base_url().to_string())
}

fn settings_base_url_for(provider: ProviderId, settings_base_url: Option<&str>) -> Option<String> {
    matches!(
        provider,
        ProviderId::OpenAiCodex | ProviderId::OpenAi | ProviderId::OpenAiCompatible
    )
    .then(|| trimmed_non_empty(settings_base_url).map(str::to_string))
    .flatten()
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
            microcompaction_watermark: None,
            default_reasoning: reasoning.map(str::to_string),
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
            open_ai_compatible: None,
            verify: None,
            tui: None,
            default_approval: None,
            worktree_root: None,
            compaction: None,
        }
    }

    #[test]
    fn codex_stream_idle_timeout_resolves_default_custom_and_disabled() {
        let default = ModelSelection::resolve(&Settings::default()).unwrap();
        assert_eq!(
            default.codex_stream_idle_timeout,
            Some(std::time::Duration::from_millis(300_000))
        );

        let custom: Settings = serde_json::from_value(serde_json::json!({
            "codexStreamIdleTimeoutMs": 123_456
        }))
        .unwrap();
        assert_eq!(
            ModelSelection::resolve(&custom)
                .unwrap()
                .codex_stream_idle_timeout,
            Some(std::time::Duration::from_millis(123_456))
        );

        let disabled: Settings = serde_json::from_value(serde_json::json!({
            "codexStreamIdleTimeoutMs": 0
        }))
        .unwrap();
        assert_eq!(
            ModelSelection::resolve(&disabled)
                .unwrap()
                .codex_stream_idle_timeout,
            None
        );
    }

    #[test]
    fn clear_tool_uses_and_microcompaction_must_be_disjoint() {
        // Both enabled -> rejected, naming both settings (issue #400,
        // ADR-0022 addendum).
        let mut s = settings(Some("anthropic"), None, None, None);
        s.anthropic_context_management = Some(serde_json::json!({
            "clearToolUses": { "triggerInputTokens": 50000 }
        }));
        s.microcompaction = Some(true);
        let error = format!("{:#}", ModelSelection::resolve(&s).unwrap_err());
        assert!(error.contains("clearToolUses"), "names the edit: {error}");
        assert!(
            error.contains("microcompaction"),
            "names the toggle: {error}"
        );

        // Either alone passes.
        let mut clear_only = settings(Some("anthropic"), None, None, None);
        clear_only.anthropic_context_management = Some(serde_json::json!({
            "clearToolUses": { "triggerInputTokens": 50000 }
        }));
        assert!(ModelSelection::resolve(&clear_only).is_ok());
        let mut micro_only = settings(Some("anthropic"), None, None, None);
        micro_only.microcompaction = Some(true);
        assert!(ModelSelection::resolve(&micro_only).is_ok());

        let mut disjoint = micro_only;
        disjoint.anthropic_context_management = Some(serde_json::json!({
            "clearToolUses": {"excludeTools": ["read", "ls"]}
        }));
        assert!(ModelSelection::resolve(&disjoint).is_ok());

        // clear_thinking is orthogonal: folds never touch thinking blocks.
        let mut thinking = settings(Some("anthropic"), None, None, None);
        thinking.anthropic_context_management = Some(serde_json::json!({
            "clearThinking": { "triggerInputTokens": 50000 }
        }));
        thinking.microcompaction = Some(true);
        assert!(ModelSelection::resolve(&thinking).is_ok());
    }

    #[test]
    fn structured_native_clearing_maps_only_when_disjoint_and_supported() {
        let base = serde_json::json!({
            "defaultProvider": "anthropic",
            "toolResultCompaction": {
                "enabled": true,
                "aggressiveness": "custom",
                "triggerTokens": 42000,
                "semanticDedupe": {
                    "enabled": true,
                    "protectRecentTokens": 2000
                },
                "toolClearing": {
                    "enabled": true,
                    "backend": "anthropicNative",
                    "mode": "allRecoverable",
                    "keepRecentToolUses": 5,
                    "clearAtLeastTokens": 7000,
                    "excludedTools": ["read", "ls", "edit", "write", "recall", "read_output"],
                    "includeFailures": true,
                    "clearToolInputs": true
                }
            }
        });
        let settings: Settings = serde_json::from_value(base.clone()).unwrap();
        let selection = ModelSelection::resolve(&settings).unwrap();
        assert!(selection.tool_result_compaction.semantic_dedupe.enabled);
        assert!(!selection.tool_result_compaction.tool_clearing.enabled);
        let native = selection
            .context_management
            .clear_tool_uses
            .expect("native clearing mapped");
        assert_eq!(native.trigger_input_tokens, Some(42_000));
        assert_eq!(native.keep_tool_uses, Some(5));
        assert_eq!(native.clear_at_least_input_tokens, Some(7_000));
        assert_eq!(native.clear_tool_inputs, Some(true));
        assert!(
            native
                .exclude_tools
                .as_ref()
                .is_some_and(|tools| tools.iter().any(|tool| tool == "read"))
        );

        let mut overlap = base;
        overlap["toolResultCompaction"]["toolClearing"]["excludedTools"] =
            serde_json::json!(["edit", "write", "recall", "read_output"]);
        let settings: Settings = serde_json::from_value(overlap).unwrap();
        let error = format!("{:#}", ModelSelection::resolve(&settings).unwrap_err());
        assert!(error.contains("overlap"), "{error}");
        assert!(error.contains("read"), "{error}");

        let mut unsupported = settings;
        unsupported.default_provider = Some("openai-codex".to_string());
        let error = format!("{:#}", ModelSelection::resolve(&unsupported).unwrap_err());
        assert!(error.contains("anthropicNative"), "{error}");
        assert!(error.contains("backend=auto"), "{error}");
    }

    #[test]
    fn auto_backend_falls_back_to_local_on_unsupported_or_overlapping_lanes() {
        for provider in ["openai-codex", "anthropic"] {
            let settings: Settings = serde_json::from_value(serde_json::json!({
                "defaultProvider": provider,
                "toolResultCompaction": {
                    "enabled": true,
                    "aggressiveness": "balanced",
                    "toolClearing": {"backend": "auto"}
                }
            }))
            .unwrap();
            let selection = ModelSelection::resolve(&settings).unwrap();
            assert!(selection.tool_result_compaction.tool_clearing.enabled);
            assert!(selection.context_management.clear_tool_uses.is_none());
        }

        let native_ready: Settings = serde_json::from_value(serde_json::json!({
            "defaultProvider": "anthropic",
            "toolResultCompaction": {
                "enabled": true,
                "aggressiveness": "custom",
                "semanticDedupe": {"enabled": false},
                "toolClearing": {
                    "enabled": true,
                    "backend": "auto",
                    "includeFailures": true
                }
            }
        }))
        .unwrap();
        let selection = ModelSelection::resolve(&native_ready).unwrap();
        assert!(!selection.tool_result_compaction.tool_clearing.enabled);
        assert!(selection.context_management.clear_tool_uses.is_some());
    }

    /// A selection literal for the cache-profile table tests, bypassing
    /// settings/env resolution.
    fn selection_for(
        provider: ProviderId,
        model: &str,
        retention: PromptCacheRetention,
    ) -> ModelSelection {
        ModelSelection {
            provider,
            model: model.to_string(),
            base_url: String::new(),
            reasoning: None,
            cache_retention: retention,
            codex_transport: CodexTransport::Auto,
            codex_stream_idle_timeout: Some(std::time::Duration::from_millis(300_000)),
            context_management: ContextManagement::default(),
            legacy_context_management: ContextManagement::default(),
            tool_result_compaction: crate::config::Settings::default()
                .tool_result_compaction()
                .unwrap(),
            configured_tool_result_compaction: crate::config::Settings::default()
                .tool_result_compaction()
                .unwrap(),
            retry_policy: crate::mimir::retry::RetryPolicy::default(),
            open_ai_compatible: OpenAiCompatibleConfig::default(),
        }
    }

    #[test]
    fn cache_profile_maps_the_four_row_table() {
        use std::time::Duration;
        // Anthropic short: 6 min cold, 1.25x writes, 1024 minimum.
        let short = cache_profile(&selection_for(
            ProviderId::Anthropic,
            "claude-sonnet-4-6",
            PromptCacheRetention::Short,
        ));
        assert_eq!(short.cold_after, Some(Duration::from_secs(6 * 60)));
        assert_eq!(short.write_premium, 1.25);
        assert_eq!(short.read_rate, 0.10);
        assert!(short.reports_writes);
        assert_eq!(short.min_cacheable_tokens, 1024);

        // Anthropic long: 72 min cold, 2x write premium (the 1h tier).
        let long = cache_profile(&selection_for(
            ProviderId::Anthropic,
            "claude-sonnet-4-6",
            PromptCacheRetention::Long,
        ));
        assert_eq!(long.cold_after, Some(Duration::from_secs(72 * 60)));
        assert_eq!(long.write_premium, 2.0);

        // Haiku-class models have the larger documented minimum.
        let haiku = cache_profile(&selection_for(
            ProviderId::Anthropic,
            "claude-haiku-4",
            PromptCacheRetention::Short,
        ));
        assert_eq!(haiku.min_cacheable_tokens, 2048);

        // OpenAI Responses lanes: 60 min guaranteed-cold bound, no write
        // premium, write side unreported.
        for provider in [ProviderId::OpenAiCodex, ProviderId::OpenAi] {
            let codex = cache_profile(&selection_for(
                provider,
                "gpt-5.5",
                PromptCacheRetention::Short,
            ));
            assert_eq!(codex.cold_after, Some(Duration::from_secs(60 * 60)));
            assert_eq!(codex.write_premium, 1.0);
            assert!(!codex.reports_writes);
            assert_eq!(codex.min_cacheable_tokens, 1024);
        }
    }

    #[test]
    fn cache_profile_degrades_unknown_lanes_and_disabled_caching_to_the_safe_default() {
        // Unknown/openai-compatible lanes: cold-based triggers off, no
        // minimum, no read discount.
        for provider in [ProviderId::Antigravity, ProviderId::OpenAiCompatible] {
            let profile = cache_profile(&selection_for(
                provider,
                "anything",
                PromptCacheRetention::Short,
            ));
            assert_eq!(profile, CacheProfile::default());
            assert_eq!(profile.cold_after, None);
            assert_eq!(profile.min_cacheable_tokens, 0);
        }
        // retention `none` disables every cache hint, so every lane degrades.
        let off = cache_profile(&selection_for(
            ProviderId::Anthropic,
            "claude-sonnet-4-6",
            PromptCacheRetention::None,
        ));
        assert_eq!(off, CacheProfile::default());
    }

    /// Env vars are process-global; serialize the env-sensitive cases through one
    /// test so concurrent test threads never observe each other's `IRIS_MODEL`.
    #[test]
    fn resolve_precedence_env_over_settings_over_default() {
        let _env = crate::mimir::test_support::env_lock();
        // Clean slate: no env overrides -> settings, then defaults.
        unsafe {
            env::remove_var("IRIS_MODEL");
            env::remove_var("IRIS_CODEX_BASE_URL");
            env::remove_var("OPENAI_API_KEY");
            env::remove_var("ANTHROPIC_API_KEY");
            env::remove_var("OPENAI_COMPATIBLE_API_KEY");
            env::remove_var("IRIS_OPENAI_COMPATIBLE_API_KEY");
        }

        // Defaults when nothing is set.
        let s = settings(None, None, None, None);
        let resolved = ModelSelection::resolve(&s).unwrap();
        assert_eq!(resolved.provider, ProviderId::OpenAiCodex);
        assert_eq!(resolved.model, "gpt-5.6-sol");
        assert_eq!(resolved.base_url, "https://chatgpt.com/backend-api");
        assert_eq!(resolved.reasoning, None);
        assert_eq!(resolved.cache_retention, PromptCacheRetention::Short);
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
        assert_eq!(ModelSelection::resolve(&s).unwrap().model, "gpt-5.6-sol");

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
            env::remove_var("OPENAI_API_KEY");
            env::remove_var("ANTHROPIC_API_KEY");
            env::remove_var("OPENAI_COMPATIBLE_API_KEY");
            env::remove_var("IRIS_OPENAI_COMPATIBLE_API_KEY");
        }
    }

    #[test]
    fn resolve_parses_default_reasoning_as_persisted_normalized_token() {
        let _env = crate::mimir::test_support::env_lock();
        unsafe {
            env::remove_var("IRIS_MODEL");
            env::remove_var("OPENAI_API_KEY");
            env::remove_var("ANTHROPIC_API_KEY");
            env::remove_var("OPENAI_COMPATIBLE_API_KEY");
            env::remove_var("IRIS_OPENAI_COMPATIBLE_API_KEY");
        }

        let adaptive = settings(
            Some("anthropic"),
            Some("claude-sonnet-5"),
            None,
            Some("medium"),
        );
        assert_eq!(
            ModelSelection::resolve(&adaptive).unwrap().reasoning,
            Some(ReasoningEffort::Medium),
            "persisted normalized tokens must round-trip before provider-native labels"
        );

        let hand_edited = settings(
            Some("anthropic"),
            Some("claude-sonnet-5"),
            None,
            Some("max"),
        );
        assert_eq!(
            ModelSelection::resolve(&hand_edited).unwrap().reasoning,
            Some(ReasoningEffort::XHigh),
            "non-normalized provider-native labels remain accepted for hand-edited settings"
        );

        let codex_5_6 = settings(Some("openai-codex"), Some("gpt-5.6-sol"), None, Some("max"));
        assert_eq!(
            ModelSelection::resolve(&codex_5_6).unwrap().reasoning,
            Some(ReasoningEffort::Max),
            "GPT-5.6 Codex settings preserve native max as a distinct effort"
        );

        let manual = settings(
            Some("anthropic"),
            Some("claude-sonnet-4-6"),
            None,
            Some("low"),
        );
        assert_eq!(
            ModelSelection::resolve(&manual).unwrap().reasoning,
            Some(ReasoningEffort::Low),
            "manual-budget models keep normalized low for legacy text"
        );
    }

    #[test]
    fn resolve_builds_retry_policy_from_settings_with_defaults() {
        use crate::config::RetrySettings;
        use crate::mimir::retry::{DEFAULT_BASE_BACKOFF, RetryPolicy};
        use std::time::Duration;

        // Unset -> the pi-mono-aligned default policy.
        let s = settings(None, None, None, None);
        assert_eq!(
            ModelSelection::resolve(&s).unwrap().retry_policy,
            RetryPolicy::default()
        );

        // Present -> resolved, with any absent subfield filled by the default.
        let mut s = settings(None, None, None, None);
        s.retry = Some(RetrySettings {
            max_retries: Some(7),
            base_delay_ms: None,
            max_delay_ms: Some(45_000),
        });
        let policy = ModelSelection::resolve(&s).unwrap().retry_policy;
        assert_eq!(policy.max_retries, 7);
        assert_eq!(policy.base_backoff, DEFAULT_BASE_BACKOFF);
        assert_eq!(policy.max_backoff, Duration::from_millis(45_000));
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

        s.default_provider = Some("anthropic".to_string());
        s.anthropic_context_management = Some(serde_json::json!({
            "clearToolUses": {
                "triggerInputTokens": 100000,
                "keepToolUses": 3,
                "excludeTools": ["read", "ls"],
                "clearToolInputs": false
            },
            "clearThinking": { "triggerInputTokens": 90000, "keepThinkingTurns": 2 },
        }));
        let cm = ModelSelection::resolve(&s).unwrap().context_management;
        assert!(cm.is_enabled());
        let clear = cm.clear_tool_uses.expect("clear_tool_uses");
        assert_eq!(clear.trigger_input_tokens, Some(100000));
        assert_eq!(clear.keep_tool_uses, Some(3));
        assert_eq!(
            clear.exclude_tools,
            Some(vec!["read".to_string(), "ls".to_string()])
        );
        assert_eq!(clear.clear_tool_inputs, Some(false));
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
        // Default is short-lived prompt caching so stable prefixes are cacheable
        // unless the user explicitly opts out.
        assert_eq!(
            ModelSelection::resolve(&s).unwrap().cache_retention,
            PromptCacheRetention::Short
        );

        // Existing users who update with a settings file that lacks
        // `promptCacheRetention` also get the new default.
        let mut existing = settings(
            Some("anthropic"),
            Some("claude-opus-4-8"),
            None,
            Some("low"),
        );
        existing.prompt_cache_retention = None;
        assert_eq!(
            ModelSelection::resolve(&existing).unwrap().cache_retention,
            PromptCacheRetention::Short
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
            ProviderId::OpenAi,
            ProviderId::Anthropic,
            ProviderId::Antigravity,
            ProviderId::OpenAiCompatible,
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
    fn resolve_defaults_to_api_key_provider_when_only_api_key_env_is_available() {
        let _env = crate::mimir::test_support::env_lock();
        unsafe {
            env::remove_var("IRIS_MODEL");
            env::remove_var("IRIS_CODEX_BASE_URL");
            env::set_var("OPENAI_API_KEY", "sk-env");
            env::remove_var("ANTHROPIC_API_KEY");
            env::remove_var("OPENAI_COMPATIBLE_API_KEY");
            env::remove_var("IRIS_OPENAI_COMPATIBLE_API_KEY");
        }
        let resolved = ModelSelection::resolve(&settings(None, None, None, None)).unwrap();
        assert_eq!(resolved.provider, ProviderId::OpenAi);
        assert_eq!(resolved.model, "gpt-4.1");
        assert_eq!(resolved.base_url, "https://api.openai.com/v1");

        unsafe {
            env::remove_var("OPENAI_API_KEY");
            env::set_var("ANTHROPIC_API_KEY", "sk-env");
        }
        let resolved = ModelSelection::resolve(&settings(None, None, None, None)).unwrap();
        assert_eq!(resolved.provider, ProviderId::Anthropic);
        assert_eq!(resolved.model, "claude-sonnet-4-6");

        unsafe {
            env::remove_var("ANTHROPIC_API_KEY");
            env::set_var("OPENAI_COMPATIBLE_API_KEY", "sk-compatible-env");
        }
        let resolved = ModelSelection::resolve(&settings(None, None, None, None)).unwrap();
        assert_eq!(resolved.provider, ProviderId::OpenAiCompatible);
        assert_eq!(resolved.model, "llama3.1");

        unsafe { env::remove_var("OPENAI_COMPATIBLE_API_KEY") };
    }

    #[test]
    fn openai_compatible_resolves_configured_model_base_url_and_flags() {
        let mut s = settings(
            Some("openai-compatible"),
            Some("llama3.1"),
            Some("http://localhost:11434/v1"),
            Some("high"),
        );
        s.open_ai_compatible = Some(OpenAiCompatibleSettings {
            context_window: Some(131_072),
            reasoning: Some(true),
            api_key_required: Some(false),
        });

        let resolved = ModelSelection::resolve(&s).unwrap();

        assert_eq!(resolved.provider, ProviderId::OpenAiCompatible);
        assert_eq!(resolved.model, "llama3.1");
        assert_eq!(resolved.base_url, "http://localhost:11434/v1");
        assert_eq!(resolved.open_ai_compatible.context_window, Some(131_072));
        assert!(resolved.open_ai_compatible.reasoning);
        assert!(!resolved.open_ai_compatible.api_key_required);
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
            "https://daily-cloudcode-pa.googleapis.com",
            "settings base URL must not redirect Google OAuth traffic"
        );
        assert_eq!(
            base_url_for(
                ProviderId::OpenAiCompatible,
                Some("http://localhost:11434/v1")
            ),
            "http://localhost:11434/v1",
            "custom OpenAI-compatible endpoints still use the configured base URL"
        );
    }

    #[test]
    fn dedicated_compaction_worker_resolves_a_qualified_global_model() {
        let settings = settings(Some("openai-codex"), Some("gpt-5.4-mini"), None, None);
        let worker =
            ModelSelection::resolve_compaction_worker(&settings, "anthropic/claude-opus-4-6")
                .unwrap();
        assert_eq!(worker.provider, ProviderId::Anthropic);
        assert_eq!(worker.model, "claude-opus-4-6");
        assert_eq!(worker.base_url, "https://api.anthropic.com");
        assert_eq!(worker.reasoning, None);

        let error = ModelSelection::resolve_compaction_worker(&settings, "claude-opus-4-6")
            .unwrap_err()
            .to_string();
        assert!(error.contains("provider/model"), "{error}");
    }
}
