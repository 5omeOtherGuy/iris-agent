//! Reasoning capability validation and provider-native display (Tier 3 Mimir):
//! one hard-coded table that says which [`ReasoningEffort`] levels a given
//! provider/model accepts, how those levels are presented to the user, plus the
//! [`clamp`] that down-maps carried unsupported levels when switching models.
//!
//! The stored/runtime value remains Iris's normalized [`ReasoningEffort`] so
//! sessions and settings can carry one compact value across providers. The picker
//! labels are provider-native: OpenAI Chat shows `low|medium|high`, Anthropic
//! adaptive models show `low|medium|high|xhigh|max`, Anthropic manual-budget
//! models show their exact `budget_tokens`, and Gemini/Antigravity shows
//! `minimal|low|medium|high`.

use anyhow::Result;

use crate::errors::UsageError;
use crate::mimir::anthropic_models::ThinkingMode;
use crate::mimir::selection::{ModelSelection, ProviderId, ReasoningEffort};

/// One selectable reasoning row: the normalized value Iris stores/applies plus
/// the provider-native label shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReasoningOption {
    pub(crate) level: ReasoningEffort,
    pub(crate) label: &'static str,
    pub(crate) detail: &'static str,
}

const fn option(
    level: ReasoningEffort,
    label: &'static str,
    detail: &'static str,
) -> ReasoningOption {
    ReasoningOption {
        level,
        label,
        detail,
    }
}

const ALL_LEVELS: &[ReasoningEffort] = &[
    ReasoningEffort::Off,
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
const STANDARD_LEVELS: &[ReasoningEffort] = &[
    ReasoningEffort::Off,
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];
const OPENAI_CHAT_LEVELS: &[ReasoningEffort] = &[
    ReasoningEffort::Off,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];

const OPENAI_CODEX_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", "omit reasoning"),
    option(
        ReasoningEffort::Minimal,
        "minimal",
        "OpenAI reasoning.effort",
    ),
    option(ReasoningEffort::Low, "low", "OpenAI reasoning.effort"),
    option(ReasoningEffort::Medium, "medium", "OpenAI reasoning.effort"),
    option(ReasoningEffort::High, "high", "OpenAI reasoning.effort"),
    option(ReasoningEffort::XHigh, "xhigh", "OpenAI reasoning.effort"),
];
const OPENAI_CHAT_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", "omit reasoning_effort"),
    option(ReasoningEffort::Low, "low", "OpenAI reasoning_effort"),
    option(ReasoningEffort::Medium, "medium", "OpenAI reasoning_effort"),
    option(ReasoningEffort::High, "high", "OpenAI reasoning_effort"),
];
const ANTIGRAVITY_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", "omit thinkingConfig"),
    option(ReasoningEffort::Minimal, "minimal", "Gemini thinkingLevel"),
    option(ReasoningEffort::Low, "low", "Gemini thinkingLevel"),
    option(ReasoningEffort::Medium, "medium", "Gemini thinkingLevel"),
    option(ReasoningEffort::High, "high", "Gemini thinkingLevel"),
];
const ANTHROPIC_MANUAL_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", "omit thinking"),
    option(
        ReasoningEffort::Minimal,
        "1,024 tokens",
        "Anthropic budget_tokens",
    ),
    option(
        ReasoningEffort::Low,
        "4,096 tokens",
        "Anthropic budget_tokens",
    ),
    option(
        ReasoningEffort::Medium,
        "10,240 tokens",
        "Anthropic budget_tokens",
    ),
    option(
        ReasoningEffort::High,
        "20,480 tokens",
        "Anthropic budget_tokens",
    ),
    option(
        ReasoningEffort::XHigh,
        "32,768 tokens",
        "Anthropic budget_tokens",
    ),
];
const ANTHROPIC_MANUAL_STANDARD_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", "omit thinking"),
    option(
        ReasoningEffort::Minimal,
        "1,024 tokens",
        "Anthropic budget_tokens",
    ),
    option(
        ReasoningEffort::Low,
        "4,096 tokens",
        "Anthropic budget_tokens",
    ),
    option(
        ReasoningEffort::Medium,
        "10,240 tokens",
        "Anthropic budget_tokens",
    ),
    option(
        ReasoningEffort::High,
        "20,480 tokens",
        "Anthropic budget_tokens",
    ),
];
const ANTHROPIC_ADAPTIVE_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", "omit adaptive thinking"),
    option(
        ReasoningEffort::Minimal,
        "low",
        "Anthropic output_config.effort",
    ),
    option(
        ReasoningEffort::Low,
        "medium",
        "Anthropic output_config.effort",
    ),
    option(
        ReasoningEffort::Medium,
        "high",
        "Anthropic output_config.effort",
    ),
    option(
        ReasoningEffort::High,
        "xhigh",
        "Anthropic output_config.effort",
    ),
    option(
        ReasoningEffort::XHigh,
        "max",
        "Anthropic output_config.effort",
    ),
];

/// Reasoning levels a provider/model natively accepts. Codex, OpenAI Chat,
/// OpenAI-compatible, and Antigravity share one set per provider; Anthropic is
/// model-specific. Unknown/custom ids fall back to conservative per-provider
/// sets.
pub(crate) fn supported_levels(provider: ProviderId, model: &str) -> &'static [ReasoningEffort] {
    match provider {
        ProviderId::OpenAiCodex => ALL_LEVELS,
        // The Chat Completions-style `reasoning_effort` parameter is
        // `low|medium|high`; `minimal`/`xhigh` are Iris carry-over values that
        // clamp at runtime but are not exposed as native choices.
        ProviderId::OpenAi | ProviderId::OpenAiCompatible => OPENAI_CHAT_LEVELS,
        ProviderId::Anthropic => anthropic_supported_levels(model),
        ProviderId::Antigravity => STANDARD_LEVELS,
    }
}

/// Provider-native selectable rows for the current model.
pub(crate) fn level_options(provider: ProviderId, model: &str) -> &'static [ReasoningOption] {
    match provider {
        ProviderId::OpenAiCodex => OPENAI_CODEX_OPTIONS,
        ProviderId::OpenAi | ProviderId::OpenAiCompatible => OPENAI_CHAT_OPTIONS,
        ProviderId::Anthropic => anthropic_level_options(model),
        ProviderId::Antigravity => ANTIGRAVITY_OPTIONS,
    }
}

/// Provider-native label for a normalized level. If the level is not native for
/// this model, fall back to the normalized token so warnings can name the user's
/// unsupported request honestly.
pub(crate) fn display_level(
    provider: ProviderId,
    model: &str,
    level: ReasoningEffort,
) -> &'static str {
    level_options(provider, model)
        .iter()
        .find(|option| option.level == level)
        .map(|option| option.label)
        .unwrap_or_else(|| level.as_str())
}

/// Parse a user-entered level in provider-native terms first, then accept legacy
/// normalized Iris tokens for compatibility with existing text commands.
pub(crate) fn parse_level(
    provider: ProviderId,
    model: &str,
    value: &str,
) -> Result<ReasoningEffort> {
    let needle = normalize_label(value);
    for option in level_options(provider, model) {
        if needle == normalize_label(option.label) || needle == option.level.as_str() {
            return Ok(option.level);
        }
    }
    if let Ok(level) = ReasoningEffort::parse(value) {
        return Ok(level);
    }
    Err(UsageError::new(format!(
        "unsupported reasoning level '{}'; supported: {}",
        value.trim(),
        join_display_levels(provider, model),
    ))
    .into())
}

fn normalize_label(value: &str) -> String {
    let mut normalized = value.trim().to_ascii_lowercase().replace(',', "");
    normalized = normalized.replace('_', "-");
    for suffix in [" tokens", " token"] {
        if let Some(stripped) = normalized.strip_suffix(suffix) {
            normalized = stripped.to_string();
            break;
        }
    }
    normalized
}

/// Anthropic supported levels, keyed by model. Every Claude Code subscription
/// model accepts the full normalized set; unknown/older non-subscription ids stay
/// conservative and top out at `high` (`xhigh` clamps to `high`).
fn anthropic_supported_levels(model: &str) -> &'static [ReasoningEffort] {
    if crate::mimir::anthropic_models::is_subscription_model(model) {
        ALL_LEVELS
    } else {
        STANDARD_LEVELS
    }
}

fn anthropic_level_options(model: &str) -> &'static [ReasoningOption] {
    match crate::mimir::anthropic_models::find(model).map(|model| model.thinking) {
        Some(ThinkingMode::Adaptive) => ANTHROPIC_ADAPTIVE_OPTIONS,
        Some(ThinkingMode::ManualBudget) => ANTHROPIC_MANUAL_OPTIONS,
        None => ANTHROPIC_MANUAL_STANDARD_OPTIONS,
    }
}

/// Whether the model exposes any thinking level beyond `off`. A non-reasoning
/// model exposes only `off`; pi-mono shows "Current model does not support
/// thinking" for those. All Iris providers support reasoning today, but this
/// stays general so a future non-reasoning model is handled.
pub(crate) fn supports_thinking(provider: ProviderId, model: &str) -> bool {
    supported_levels(provider, model)
        .iter()
        .any(|level| *level != ReasoningEffort::Off)
}

/// Advance to the next available thinking level for the model, with wraparound,
/// matching pi-mono's `app.thinking.cycle`. `forward` walks toward higher
/// effort; `false` walks back. Returns `None` when the model does not support
/// thinking. A `current` level the model does not natively expose is first
/// clamped onto the supported set, then advanced.
pub(crate) fn cycle_effort(
    provider: ProviderId,
    model: &str,
    current: ReasoningEffort,
    forward: bool,
) -> Option<ReasoningEffort> {
    if !supports_thinking(provider, model) {
        return None;
    }
    let levels = supported_levels(provider, model);
    let clamped = clamp(provider, model, current);
    let idx = levels
        .iter()
        .position(|level| *level == clamped)
        .unwrap_or(0);
    let len = levels.len();
    let next = if forward {
        (idx + 1) % len
    } else {
        (idx + len - 1) % len
    };
    Some(levels[next])
}

/// Validate an explicit reasoning preference against the active model. `None`
/// always passes (no preference -> today's wire). `Some(level)` errors with an
/// actionable message when the level is not natively supported, so a configured
/// `defaultReasoning` that the model rejects fails loudly at startup.
pub(crate) fn validate(selection: &ModelSelection) -> Result<()> {
    let Some(level) = selection.reasoning else {
        return Ok(());
    };
    if selection.provider == ProviderId::OpenAiCompatible
        && !selection.open_ai_compatible.reasoning
        && level != ReasoningEffort::Off
    {
        return Err(UsageError::new(format!(
            "reasoning level '{}' is not enabled for openai-compatible/{}; set openAiCompatible.reasoning to true or use 'off'",
            level.as_str(), selection.model
        ))
        .into());
    }
    let supported = supported_levels(selection.provider, &selection.model);
    if supported.contains(&level) {
        return Ok(());
    }
    Err(UsageError::new(format!(
        "reasoning level '{}' is not supported by {}/{}; supported: {}",
        level.as_str(),
        selection.provider.as_str(),
        selection.model,
        join_display_levels(selection.provider, &selection.model),
    ))
    .into())
}

/// Down-map unsupported carry-over levels to the nearest provider-native one.
/// This preserves an in-session model switch (for example Codex `minimal` ->
/// OpenAI Chat `low`, Codex `xhigh` -> OpenAI Chat/Gemini `high`) while keeping
/// explicit settings validation strict.
pub(crate) fn clamp(provider: ProviderId, model: &str, level: ReasoningEffort) -> ReasoningEffort {
    let supported = supported_levels(provider, model);
    if supported.contains(&level) {
        return level;
    }
    if level == ReasoningEffort::Minimal && supported.contains(&ReasoningEffort::Low) {
        return ReasoningEffort::Low;
    }
    if level == ReasoningEffort::XHigh && supported.contains(&ReasoningEffort::High) {
        return ReasoningEffort::High;
    }
    level
}

/// Comma-join provider-native level labels for user-facing messages.
pub(crate) fn join_display_levels(provider: ProviderId, model: &str) -> String {
    level_options(provider, model)
        .iter()
        .map(|option| option.label)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mimir::selection::{ContextManagement, PromptCacheRetention};

    fn selection(provider: ProviderId, reasoning: Option<ReasoningEffort>) -> ModelSelection {
        ModelSelection {
            provider,
            model: "m".to_string(),
            base_url: "https://example".to_string(),
            reasoning,
            cache_retention: PromptCacheRetention::Short,
            context_management: ContextManagement::default(),
            retry_policy: crate::mimir::retry::RetryPolicy::default(),
            open_ai_compatible: crate::mimir::selection::OpenAiCompatibleConfig::default(),
        }
    }

    #[test]
    fn validate_passes_for_none_and_supported_levels() {
        assert!(validate(&selection(ProviderId::Anthropic, None)).is_ok());
        assert!(
            validate(&selection(
                ProviderId::OpenAiCodex,
                Some(ReasoningEffort::XHigh)
            ))
            .is_ok()
        );
        assert!(validate(&selection(ProviderId::OpenAi, Some(ReasoningEffort::High))).is_ok());
        assert!(
            validate(&selection(
                ProviderId::Anthropic,
                Some(ReasoningEffort::High)
            ))
            .is_ok()
        );
    }

    #[test]
    fn validate_rejects_unsupported_level_with_actionable_message() {
        let err = validate(&selection(
            ProviderId::Anthropic,
            Some(ReasoningEffort::XHigh),
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("xhigh"), "{err}");
        assert!(err.contains("anthropic"), "{err}");
        assert!(
            err.contains("off, 1,024 tokens, 4,096 tokens, 10,240 tokens, 20,480 tokens"),
            "{err}"
        );

        let err = validate(&selection(
            ProviderId::OpenAi,
            Some(ReasoningEffort::Minimal),
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("supported: off, low, medium, high"), "{err}");
    }

    #[test]
    fn provider_native_options_match_wire_names() {
        assert_eq!(
            join_display_levels(ProviderId::OpenAi, "gpt-4.1"),
            "off, low, medium, high"
        );
        assert_eq!(
            join_display_levels(ProviderId::OpenAiCodex, "gpt-5.5"),
            "off, minimal, low, medium, high, xhigh"
        );
        assert_eq!(
            join_display_levels(ProviderId::Anthropic, "claude-sonnet-5"),
            "off, low, medium, high, xhigh, max"
        );
        assert_eq!(
            join_display_levels(ProviderId::Anthropic, "claude-sonnet-4-6"),
            "off, 1,024 tokens, 4,096 tokens, 10,240 tokens, 20,480 tokens, 32,768 tokens"
        );
        assert_eq!(
            display_level(
                ProviderId::Anthropic,
                "claude-sonnet-5",
                ReasoningEffort::XHigh
            ),
            "max"
        );
    }

    #[test]
    fn parse_level_prefers_provider_native_labels() {
        // Anthropic adaptive `low` is the provider's lowest effort; internally it
        // maps to Iris Minimal so the request builder sends output_config.effort=low.
        assert_eq!(
            parse_level(ProviderId::Anthropic, "claude-sonnet-5", "low").unwrap(),
            ReasoningEffort::Minimal
        );
        assert_eq!(
            parse_level(ProviderId::Anthropic, "claude-sonnet-5", "max").unwrap(),
            ReasoningEffort::XHigh
        );
        assert_eq!(
            parse_level(ProviderId::Anthropic, "claude-sonnet-4-6", "4096").unwrap(),
            ReasoningEffort::Low
        );
        assert_eq!(
            parse_level(ProviderId::Anthropic, "claude-sonnet-4-6", "4,096 tokens").unwrap(),
            ReasoningEffort::Low
        );
    }

    #[test]
    fn anthropic_xhigh_is_model_specific() {
        // The shipped subscription models accept xhigh natively (it maps up to
        // Anthropic's `max`/`xhigh` effort or the `xhigh` 32768 budget): validate
        // passes, clamp is identity.
        for model in [
            "claude-sonnet-5",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
        ] {
            let sel = ModelSelection {
                provider: ProviderId::Anthropic,
                model: model.to_string(),
                base_url: "https://example".to_string(),
                reasoning: Some(ReasoningEffort::XHigh),
                cache_retention: PromptCacheRetention::Short,
                context_management: ContextManagement::default(),
                retry_policy: crate::mimir::retry::RetryPolicy::default(),
                open_ai_compatible: crate::mimir::selection::OpenAiCompatibleConfig::default(),
            };
            assert!(validate(&sel).is_ok(), "{model} should accept xhigh");
            assert_eq!(
                clamp(ProviderId::Anthropic, model, ReasoningEffort::XHigh),
                ReasoningEffort::XHigh,
                "{model} keeps xhigh"
            );
        }
        // Unknown/older budget-based ids top out at high: xhigh clamps down.
        assert_eq!(
            clamp(
                ProviderId::Anthropic,
                "claude-3-7-sonnet",
                ReasoningEffort::XHigh
            ),
            ReasoningEffort::High
        );
    }

    #[test]
    fn cycle_effort_wraps_within_supported_levels() {
        // Codex supports off..xhigh (6 levels): high -> xhigh -> wrap to off.
        assert_eq!(
            cycle_effort(
                ProviderId::OpenAiCodex,
                "gpt-5.5",
                ReasoningEffort::High,
                true
            ),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            cycle_effort(
                ProviderId::OpenAiCodex,
                "gpt-5.5",
                ReasoningEffort::XHigh,
                true
            ),
            Some(ReasoningEffort::Off)
        );
        // Backward from off wraps to the top (xhigh).
        assert_eq!(
            cycle_effort(
                ProviderId::OpenAiCodex,
                "gpt-5.5",
                ReasoningEffort::Off,
                false
            ),
            Some(ReasoningEffort::XHigh)
        );
        // OpenAI Chat exposes only off/low/medium/high; high wraps to off.
        assert_eq!(
            cycle_effort(ProviderId::OpenAi, "gpt-4.1", ReasoningEffort::High, true),
            Some(ReasoningEffort::Off)
        );
        // Anthropic Sonnet 4.6 supports xhigh as its top manual budget, so the
        // forward step from xhigh wraps around to off.
        assert_eq!(
            cycle_effort(
                ProviderId::Anthropic,
                "claude-sonnet-4-6",
                ReasoningEffort::XHigh,
                true
            ),
            Some(ReasoningEffort::Off)
        );
    }

    #[test]
    fn supports_thinking_is_true_for_reasoning_providers() {
        assert!(supports_thinking(ProviderId::OpenAiCodex, "gpt-5.5"));
        assert!(supports_thinking(ProviderId::OpenAi, "gpt-4.1"));
        assert!(supports_thinking(
            ProviderId::Antigravity,
            "gemini-3.5-flash"
        ));
        assert!(supports_thinking(
            ProviderId::Anthropic,
            "claude-sonnet-4-6"
        ));
    }

    #[test]
    fn clamp_down_maps_only_where_unsupported() {
        // OpenAI Chat exposes low/medium/high: carry-over minimal/xhigh clamp to
        // the nearest native endpoint levels.
        assert_eq!(
            clamp(ProviderId::OpenAi, "gpt-4.1", ReasoningEffort::Minimal),
            ReasoningEffort::Low
        );
        assert_eq!(
            clamp(ProviderId::OpenAi, "gpt-4.1", ReasoningEffort::XHigh),
            ReasoningEffort::High
        );
        // Anthropic (older/unknown ids) / Antigravity: xhigh -> high.
        assert_eq!(
            clamp(
                ProviderId::Anthropic,
                "claude-3-7-sonnet",
                ReasoningEffort::XHigh
            ),
            ReasoningEffort::High
        );
        assert_eq!(
            clamp(
                ProviderId::Antigravity,
                "gemini-3.5-flash",
                ReasoningEffort::XHigh
            ),
            ReasoningEffort::High
        );
        // Codex supports xhigh natively: identity.
        assert_eq!(
            clamp(ProviderId::OpenAiCodex, "gpt-5.5", ReasoningEffort::XHigh),
            ReasoningEffort::XHigh
        );
        // A supported level is unchanged everywhere.
        assert_eq!(
            clamp(
                ProviderId::Anthropic,
                "claude-sonnet-4-6",
                ReasoningEffort::Medium
            ),
            ReasoningEffort::Medium
        );
    }
}
