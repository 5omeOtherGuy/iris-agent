//! Reasoning capability validation and provider-native display (Tier 3 Mimir):
//! one hard-coded table that says which [`ReasoningEffort`] levels a given
//! provider/model accepts, how those levels are presented to the user, plus the
//! [`clamp`] that down-maps carried unsupported levels when switching models.
//!
//! The stored/runtime value remains Iris's normalized [`ReasoningEffort`] so
//! sessions and settings can carry one compact value across providers. The picker
//! labels are provider-native enough to explain the wire effect: OpenAI API
//! non-reasoning chat models show only `off`, OpenAI-compatible reasoning shows
//! `low|medium|high`, Anthropic adaptive models show `low|medium|high|xhigh|max`,
//! Anthropic manual-budget models show their exact `budget_tokens`, and
//! Gemini/Antigravity shows its model-specific `thinkingLevel` mapping.

use anyhow::Result;

use crate::errors::UsageError;
use crate::mimir::anthropic_models::ThinkingMode;
use crate::mimir::selection::{ModelSelection, ProviderId, ReasoningEffort};

/// Typed provider request shape for one supported reasoning level. Provider
/// adapters consume this directly, so picker labels, validation, and wire
/// payloads cannot drift into separate capability tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReasoningWire {
    /// No reasoning field is sent (`off`, or a model with no reasoning support).
    Omit,
    OpenAiResponses {
        effort: &'static str,
        summary: &'static str,
    },
    OpenAiChatCompletions {
        effort: &'static str,
    },
    AnthropicManual {
        budget_tokens: u32,
    },
    AnthropicAdaptive {
        effort: &'static str,
    },
    Gemini {
        thinking_level: &'static str,
        include_thoughts: bool,
    },
}

impl ReasoningWire {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Omit => "reasoning omitted",
            Self::OpenAiResponses { effort: "low", .. } => "OpenAI reasoning.effort low",
            Self::OpenAiResponses {
                effort: "medium", ..
            } => "OpenAI reasoning.effort medium",
            Self::OpenAiResponses { effort: "high", .. } => "OpenAI reasoning.effort high",
            Self::OpenAiResponses {
                effort: "xhigh", ..
            } => "OpenAI reasoning.effort xhigh",
            Self::OpenAiResponses { effort: "max", .. } => "OpenAI reasoning.effort max",
            Self::OpenAiResponses { .. } => "OpenAI Responses reasoning.effort",
            Self::OpenAiChatCompletions { effort: "low" } => "OpenAI reasoning_effort low",
            Self::OpenAiChatCompletions { effort: "medium" } => "OpenAI reasoning_effort medium",
            Self::OpenAiChatCompletions { effort: "high" } => "OpenAI reasoning_effort high",
            Self::OpenAiChatCompletions { .. } => "OpenAI reasoning_effort",
            Self::AnthropicManual {
                budget_tokens: 1_024,
            } => "Anthropic budget_tokens 1,024",
            Self::AnthropicManual {
                budget_tokens: 4_096,
            } => "Anthropic budget_tokens 4,096",
            Self::AnthropicManual {
                budget_tokens: 10_240,
            } => "Anthropic budget_tokens 10,240",
            Self::AnthropicManual {
                budget_tokens: 20_480,
            } => "Anthropic budget_tokens 20,480",
            Self::AnthropicManual {
                budget_tokens: 32_768,
            } => "Anthropic budget_tokens 32,768",
            Self::AnthropicManual { .. } => "Anthropic thinking.budget_tokens",
            Self::AnthropicAdaptive { effort: "low" } => "Anthropic output_config.effort low",
            Self::AnthropicAdaptive { effort: "medium" } => "Anthropic output_config.effort medium",
            Self::AnthropicAdaptive { effort: "high" } => "Anthropic output_config.effort high",
            Self::AnthropicAdaptive { effort: "xhigh" } => "Anthropic output_config.effort xhigh",
            Self::AnthropicAdaptive { effort: "max" } => "Anthropic output_config.effort max",
            Self::AnthropicAdaptive { .. } => "Anthropic adaptive output_config.effort",
            Self::Gemini {
                thinking_level: "minimal",
                ..
            } => "Gemini thinkingLevel minimal",
            Self::Gemini {
                thinking_level: "low",
                ..
            } => "Gemini thinkingLevel low",
            Self::Gemini {
                thinking_level: "medium",
                ..
            } => "Gemini thinkingLevel medium",
            Self::Gemini {
                thinking_level: "high",
                ..
            } => "Gemini thinkingLevel high",
            Self::Gemini { .. } => "Gemini generationConfig.thinkingConfig",
        }
    }
}

/// One selectable reasoning row: normalized storage value, provider-native UI
/// label, human-readable wire behavior, and the typed request mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReasoningOption {
    pub(crate) level: ReasoningEffort,
    pub(crate) label: &'static str,
    pub(crate) wire: ReasoningWire,
}

const fn option(
    level: ReasoningEffort,
    label: &'static str,
    wire: ReasoningWire,
) -> ReasoningOption {
    ReasoningOption { level, label, wire }
}

/// The complete capability returned for one provider/model. All consumers use
/// this map rather than maintaining their own reasoning-level translations.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReasoningCapability {
    pub(crate) options: &'static [ReasoningOption],
}

const OPENAI_CODEX_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", ReasoningWire::Omit),
    option(
        ReasoningEffort::Minimal,
        "minimal",
        ReasoningWire::OpenAiResponses {
            effort: "low",
            summary: "auto",
        },
    ),
    option(
        ReasoningEffort::Low,
        "low",
        ReasoningWire::OpenAiResponses {
            effort: "low",
            summary: "auto",
        },
    ),
    option(
        ReasoningEffort::Medium,
        "medium",
        ReasoningWire::OpenAiResponses {
            effort: "medium",
            summary: "auto",
        },
    ),
    option(
        ReasoningEffort::High,
        "high",
        ReasoningWire::OpenAiResponses {
            effort: "high",
            summary: "auto",
        },
    ),
    option(
        ReasoningEffort::XHigh,
        "xhigh",
        ReasoningWire::OpenAiResponses {
            effort: "xhigh",
            summary: "auto",
        },
    ),
];
const OPENAI_CODEX_5_6_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", ReasoningWire::Omit),
    option(
        ReasoningEffort::Minimal,
        "minimal",
        ReasoningWire::OpenAiResponses {
            effort: "low",
            summary: "auto",
        },
    ),
    option(
        ReasoningEffort::Low,
        "low",
        ReasoningWire::OpenAiResponses {
            effort: "low",
            summary: "auto",
        },
    ),
    option(
        ReasoningEffort::Medium,
        "medium",
        ReasoningWire::OpenAiResponses {
            effort: "medium",
            summary: "auto",
        },
    ),
    option(
        ReasoningEffort::High,
        "high",
        ReasoningWire::OpenAiResponses {
            effort: "high",
            summary: "auto",
        },
    ),
    option(
        ReasoningEffort::XHigh,
        "xhigh",
        ReasoningWire::OpenAiResponses {
            effort: "xhigh",
            summary: "auto",
        },
    ),
    option(
        ReasoningEffort::Max,
        "max",
        ReasoningWire::OpenAiResponses {
            effort: "max",
            summary: "auto",
        },
    ),
];
const OPENAI_NO_REASONING_OPTIONS: &[ReasoningOption] =
    &[option(ReasoningEffort::Off, "off", ReasoningWire::Omit)];
const OPENAI_CHAT_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", ReasoningWire::Omit),
    option(
        ReasoningEffort::Low,
        "low",
        ReasoningWire::OpenAiChatCompletions { effort: "low" },
    ),
    option(
        ReasoningEffort::Medium,
        "medium",
        ReasoningWire::OpenAiChatCompletions { effort: "medium" },
    ),
    option(
        ReasoningEffort::High,
        "high",
        ReasoningWire::OpenAiChatCompletions { effort: "high" },
    ),
];
const ANTIGRAVITY_FLASH_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", ReasoningWire::Omit),
    option(
        ReasoningEffort::Minimal,
        "minimal",
        ReasoningWire::Gemini {
            thinking_level: "minimal",
            include_thoughts: true,
        },
    ),
    option(
        ReasoningEffort::Low,
        "low",
        ReasoningWire::Gemini {
            thinking_level: "low",
            include_thoughts: true,
        },
    ),
    option(
        ReasoningEffort::Medium,
        "medium",
        ReasoningWire::Gemini {
            thinking_level: "medium",
            include_thoughts: true,
        },
    ),
    option(
        ReasoningEffort::High,
        "high",
        ReasoningWire::Gemini {
            thinking_level: "high",
            include_thoughts: true,
        },
    ),
];
const ANTIGRAVITY_PRO_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", ReasoningWire::Omit),
    option(
        ReasoningEffort::Minimal,
        "minimal",
        ReasoningWire::Gemini {
            thinking_level: "low",
            include_thoughts: true,
        },
    ),
    option(
        ReasoningEffort::Low,
        "low",
        ReasoningWire::Gemini {
            thinking_level: "low",
            include_thoughts: true,
        },
    ),
    option(
        ReasoningEffort::Medium,
        "medium",
        ReasoningWire::Gemini {
            thinking_level: "high",
            include_thoughts: true,
        },
    ),
    option(
        ReasoningEffort::High,
        "high",
        ReasoningWire::Gemini {
            thinking_level: "high",
            include_thoughts: true,
        },
    ),
];
const ANTHROPIC_MANUAL_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", ReasoningWire::Omit),
    option(
        ReasoningEffort::Minimal,
        "1,024 tokens",
        ReasoningWire::AnthropicManual {
            budget_tokens: 1_024,
        },
    ),
    option(
        ReasoningEffort::Low,
        "4,096 tokens",
        ReasoningWire::AnthropicManual {
            budget_tokens: 4_096,
        },
    ),
    option(
        ReasoningEffort::Medium,
        "10,240 tokens",
        ReasoningWire::AnthropicManual {
            budget_tokens: 10_240,
        },
    ),
    option(
        ReasoningEffort::High,
        "20,480 tokens",
        ReasoningWire::AnthropicManual {
            budget_tokens: 20_480,
        },
    ),
    option(
        ReasoningEffort::XHigh,
        "32,768 tokens",
        ReasoningWire::AnthropicManual {
            budget_tokens: 32_768,
        },
    ),
];
const ANTHROPIC_MANUAL_STANDARD_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", ReasoningWire::Omit),
    option(
        ReasoningEffort::Minimal,
        "1,024 tokens",
        ReasoningWire::AnthropicManual {
            budget_tokens: 1_024,
        },
    ),
    option(
        ReasoningEffort::Low,
        "4,096 tokens",
        ReasoningWire::AnthropicManual {
            budget_tokens: 4_096,
        },
    ),
    option(
        ReasoningEffort::Medium,
        "10,240 tokens",
        ReasoningWire::AnthropicManual {
            budget_tokens: 10_240,
        },
    ),
    option(
        ReasoningEffort::High,
        "20,480 tokens",
        ReasoningWire::AnthropicManual {
            budget_tokens: 20_480,
        },
    ),
];
const ANTHROPIC_ADAPTIVE_OPTIONS: &[ReasoningOption] = &[
    option(ReasoningEffort::Off, "off", ReasoningWire::Omit),
    option(
        ReasoningEffort::Minimal,
        "low",
        ReasoningWire::AnthropicAdaptive { effort: "low" },
    ),
    option(
        ReasoningEffort::Low,
        "medium",
        ReasoningWire::AnthropicAdaptive { effort: "medium" },
    ),
    option(
        ReasoningEffort::Medium,
        "high",
        ReasoningWire::AnthropicAdaptive { effort: "high" },
    ),
    option(
        ReasoningEffort::High,
        "xhigh",
        ReasoningWire::AnthropicAdaptive { effort: "xhigh" },
    ),
    option(
        ReasoningEffort::XHigh,
        "max",
        ReasoningWire::AnthropicAdaptive { effort: "max" },
    ),
];

/// Ordered normalization scale used only when carrying an effort across models.
const ORDERED_LEVELS: &[ReasoningEffort] = &[
    ReasoningEffort::Off,
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

/// Single provider/model capability lookup used by validation, UI, switching,
/// and every provider request adapter.
pub(crate) fn capability(provider: ProviderId, model: &str) -> ReasoningCapability {
    let options = match provider {
        ProviderId::OpenAiCodex if is_openai_codex_5_6_model(model) => OPENAI_CODEX_5_6_OPTIONS,
        ProviderId::OpenAiCodex => OPENAI_CODEX_OPTIONS,
        ProviderId::OpenAi if openai_api_supports_reasoning(model) => OPENAI_CHAT_OPTIONS,
        ProviderId::OpenAi => OPENAI_NO_REASONING_OPTIONS,
        ProviderId::OpenAiCompatible => OPENAI_CHAT_OPTIONS,
        ProviderId::Anthropic => {
            match crate::mimir::anthropic_models::find(model).map(|model| model.thinking) {
                Some(ThinkingMode::Adaptive) => ANTHROPIC_ADAPTIVE_OPTIONS,
                Some(ThinkingMode::ManualBudget) => ANTHROPIC_MANUAL_OPTIONS,
                None => ANTHROPIC_MANUAL_STANDARD_OPTIONS,
            }
        }
        ProviderId::Antigravity if is_antigravity_pro_model(model) => ANTIGRAVITY_PRO_OPTIONS,
        ProviderId::Antigravity => ANTIGRAVITY_FLASH_OPTIONS,
    };
    ReasoningCapability { options }
}

/// Reasoning levels accepted by a provider/model, derived from the typed map.
pub(crate) fn supported_levels(provider: ProviderId, model: &str) -> Vec<ReasoningEffort> {
    capability(provider, model)
        .options
        .iter()
        .map(|option| option.level)
        .collect()
}

/// Provider-native selectable rows for the current model.
pub(crate) fn level_options(provider: ProviderId, model: &str) -> &'static [ReasoningOption] {
    capability(provider, model).options
}

/// Selectable rows after applying the OpenAI-compatible endpoint's explicit
/// reasoning opt-in. The provider/model map remains the wire source of truth;
/// this boundary gate prevents disabled custom endpoints from advertising it.
pub(crate) fn selectable_options(
    provider: ProviderId,
    model: &str,
    open_ai_compatible_reasoning: bool,
) -> &'static [ReasoningOption] {
    if provider == ProviderId::OpenAiCompatible && !open_ai_compatible_reasoning {
        OPENAI_NO_REASONING_OPTIONS
    } else {
        level_options(provider, model)
    }
}

/// Typed request shape for an exactly supported level. `Off` and unsupported
/// values both return `None`, ensuring adapters omit rather than silently send a
/// field their active model does not advertise.
pub(crate) fn wire_config(
    provider: ProviderId,
    model: &str,
    level: ReasoningEffort,
) -> Option<ReasoningWire> {
    let wire = level_options(provider, model)
        .iter()
        .find(|option| option.level == level)?
        .wire;
    (wire != ReasoningWire::Omit).then_some(wire)
}

/// Human-readable request behavior for status surfaces.
pub(crate) fn wire_behavior(
    provider: ProviderId,
    model: &str,
    level: ReasoningEffort,
) -> &'static str {
    level_options(provider, model)
        .iter()
        .find(|option| option.level == level)
        .map(|option| option.wire.description())
        .unwrap_or("unsupported (reasoning omitted)")
}

fn is_openai_codex_5_6_model(model: &str) -> bool {
    matches!(model, "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna")
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

/// Parse a persisted settings/session reasoning value. Iris writes normalized
/// [`ReasoningEffort::as_str`] tokens, while interactive input uses provider-
/// native labels. Some providers (notably Anthropic adaptive thinking) have
/// labels that overlap normalized tokens but mean a different internal level, so
/// persisted values must prefer the normalized interpretation and only fall back
/// to provider-native parsing for hand-edited values like `max` or `4,096`.
pub(crate) fn parse_persisted_level(
    provider: ProviderId,
    model: &str,
    value: &str,
) -> Result<ReasoningEffort> {
    // Anthropic has long used its native `max` label for Iris's existing
    // `xhigh` normalized tier. GPT-5.6 Codex adds a distinct normalized `max`,
    // so resolve this one ambiguous token through the model-native table first
    // and preserve existing Anthropic settings.
    if value.trim().eq_ignore_ascii_case("max") {
        return parse_level(provider, model, value);
    }
    if let Ok(level) = ReasoningEffort::parse(value) {
        return Ok(level);
    }
    parse_level(provider, model, value)
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

/// Whether Iris's OpenAI API Chat Completions lane may send
/// `reasoning_effort` for this model. The built-in OpenAI catalog is currently
/// gpt-4.1/gpt-4o chat models, which reject that parameter; keep this allowlist
/// narrow and extend it with request-shape tests when adding reasoning models.
pub(crate) fn openai_api_supports_reasoning(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    matches!(
        model.as_str(),
        "o1" | "o1-mini" | "o1-preview" | "o3" | "o3-mini" | "o4-mini"
    )
}

pub(crate) fn is_antigravity_pro_model(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    model == "gemini-pro-agent" || model.contains("-pro") || model.ends_with("pro")
}

/// Whether the model exposes any thinking level beyond `off`. A non-reasoning
/// model exposes only `off`; pi-mono shows "Current model does not support
/// thinking" for those.
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
/// OpenAI Chat `low`, Codex `xhigh` -> OpenAI Chat/Gemini `high`, any explicit
/// reasoning -> `off` on non-reasoning models) while keeping explicit settings
/// validation strict.
pub(crate) fn clamp(provider: ProviderId, model: &str, level: ReasoningEffort) -> ReasoningEffort {
    let supported = supported_levels(provider, model);
    if supported.contains(&level) {
        return level;
    }
    let Some(idx) = ORDERED_LEVELS
        .iter()
        .position(|candidate| *candidate == level)
    else {
        return supported.first().copied().unwrap_or(level);
    };
    for candidate in &ORDERED_LEVELS[idx..] {
        if supported.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in ORDERED_LEVELS[..idx].iter().rev() {
        if supported.contains(candidate) {
            return *candidate;
        }
    }
    supported.first().copied().unwrap_or(level)
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
    use crate::mimir::selection::{CodexTransport, ContextManagement, PromptCacheRetention};

    fn selection(provider: ProviderId, reasoning: Option<ReasoningEffort>) -> ModelSelection {
        ModelSelection {
            provider,
            model: "m".to_string(),
            base_url: "https://example".to_string(),
            reasoning,
            cache_retention: PromptCacheRetention::Short,
            codex_transport: CodexTransport::Auto,
            context_management: ContextManagement::default(),
            legacy_context_management: ContextManagement::default(),
            tool_result_compaction: crate::config::Settings::default()
                .tool_result_compaction()
                .unwrap(),
            configured_tool_result_compaction: crate::config::Settings::default()
                .tool_result_compaction()
                .unwrap(),
            retry_policy: crate::mimir::retry::RetryPolicy::default(),
            open_ai_compatible: crate::mimir::selection::OpenAiCompatibleConfig::default(),
        }
    }

    #[test]
    fn capability_map_is_the_typed_source_for_provider_wire_shapes() {
        assert_eq!(
            wire_config(ProviderId::OpenAiCodex, "gpt-5.6-sol", ReasoningEffort::Max),
            Some(ReasoningWire::OpenAiResponses {
                effort: "max",
                summary: "auto",
            })
        );
        assert_eq!(
            wire_config(ProviderId::OpenAi, "o3", ReasoningEffort::Medium),
            Some(ReasoningWire::OpenAiChatCompletions { effort: "medium" })
        );
        assert_eq!(
            wire_config(
                ProviderId::OpenAiCompatible,
                "custom-model",
                ReasoningEffort::Medium
            ),
            Some(ReasoningWire::OpenAiChatCompletions { effort: "medium" })
        );
        assert_eq!(
            wire_config(
                ProviderId::Anthropic,
                "claude-sonnet-4-6",
                ReasoningEffort::High
            ),
            Some(ReasoningWire::AnthropicManual {
                budget_tokens: 20_480,
            })
        );
        assert_eq!(
            wire_config(
                ProviderId::Anthropic,
                "claude-sonnet-5",
                ReasoningEffort::High
            ),
            Some(ReasoningWire::AnthropicAdaptive { effort: "xhigh" })
        );
        assert_eq!(
            wire_config(
                ProviderId::Antigravity,
                "gemini-3.1-pro",
                ReasoningEffort::Minimal
            ),
            Some(ReasoningWire::Gemini {
                thinking_level: "low",
                include_thoughts: true,
            })
        );
    }

    #[test]
    fn capability_map_omits_off_and_rejects_unsupported_wire_levels() {
        assert_eq!(
            wire_config(ProviderId::OpenAiCodex, "gpt-5.5", ReasoningEffort::Off),
            None
        );
        assert_eq!(
            wire_config(ProviderId::OpenAiCodex, "gpt-5.5", ReasoningEffort::Max),
            None,
            "max must not be silently sent to a pre-5.6 Codex model"
        );
        assert_eq!(
            wire_config(ProviderId::OpenAi, "gpt-4.1", ReasoningEffort::High),
            None,
            "non-reasoning OpenAI chat models omit reasoning_effort"
        );
        assert_eq!(
            wire_config(
                ProviderId::Anthropic,
                "claude-3-7-sonnet",
                ReasoningEffort::XHigh
            ),
            None,
            "unknown/older Anthropic ids top out at high"
        );
    }

    #[test]
    fn openai_compatible_selectable_options_follow_the_explicit_gate() {
        assert_eq!(
            selectable_options(ProviderId::OpenAiCompatible, "custom", false),
            OPENAI_NO_REASONING_OPTIONS
        );
        assert_eq!(
            selectable_options(ProviderId::OpenAiCompatible, "custom", true),
            OPENAI_CHAT_OPTIONS
        );
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
        assert!(validate(&selection(ProviderId::OpenAi, Some(ReasoningEffort::Off))).is_ok());
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
        assert!(err.contains("supported: off"), "{err}");
    }

    #[test]
    fn provider_native_options_match_wire_names() {
        assert_eq!(join_display_levels(ProviderId::OpenAi, "gpt-4.1"), "off");
        assert_eq!(
            join_display_levels(ProviderId::OpenAiCompatible, "gpt-test"),
            "off, low, medium, high"
        );
        assert_eq!(
            join_display_levels(ProviderId::OpenAiCodex, "gpt-5.5"),
            "off, minimal, low, medium, high, xhigh"
        );
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert_eq!(
                join_display_levels(ProviderId::OpenAiCodex, model),
                "off, minimal, low, medium, high, xhigh, max",
                "{model} must expose its native max effort in every picker"
            );
        }
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
        assert_eq!(
            level_options(ProviderId::Antigravity, "gemini-3.1-pro")
                .iter()
                .find(|option| option.level == ReasoningEffort::Minimal)
                .expect("minimal option")
                .wire
                .description(),
            "Gemini thinkingLevel low"
        );
        assert_eq!(
            level_options(ProviderId::Antigravity, "gemini-3.1-pro")
                .iter()
                .find(|option| option.level == ReasoningEffort::Medium)
                .expect("medium option")
                .wire
                .description(),
            "Gemini thinkingLevel high"
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
    fn adaptive_label_round_trips_through_parse_and_display() {
        // Regression for issue #512: the status footer must render the provider
        // -native label the user selected, not the internal normalized token.
        // For Anthropic adaptive models the two diverge by one notch (display
        // `high` maps to internal `Medium`), so a footer built from
        // `ReasoningEffort::as_str()` shows `medium` for a `/reasoning high`
        // request. `display_level` must invert `parse_level` for every label.
        let model = "claude-sonnet-5";
        for option in level_options(ProviderId::Anthropic, model) {
            let parsed = parse_level(ProviderId::Anthropic, model, option.label).unwrap();
            assert_eq!(
                parsed, option.level,
                "parse round-trip for {}",
                option.label
            );
            assert_eq!(
                display_level(ProviderId::Anthropic, model, parsed),
                option.label,
                "display round-trip for {}",
                option.label
            );
        }
        // The specific issue-#512 case: `high` never displays as `medium`.
        let high = parse_level(ProviderId::Anthropic, model, "high").unwrap();
        assert_eq!(high, ReasoningEffort::Medium);
        assert_eq!(display_level(ProviderId::Anthropic, model, high), "high");
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
                codex_transport: CodexTransport::Auto,
                context_management: ContextManagement::default(),
                legacy_context_management: ContextManagement::default(),
                tool_result_compaction: crate::config::Settings::default()
                    .tool_result_compaction()
                    .unwrap(),
                configured_tool_result_compaction: crate::config::Settings::default()
                    .tool_result_compaction()
                    .unwrap(),
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
        // GPT-5.6 Codex models support off..max: xhigh -> max -> wrap to off.
        assert_eq!(
            cycle_effort(
                ProviderId::OpenAiCodex,
                "gpt-5.6-sol",
                ReasoningEffort::XHigh,
                true
            ),
            Some(ReasoningEffort::Max)
        );
        assert_eq!(
            cycle_effort(
                ProviderId::OpenAiCodex,
                "gpt-5.6-sol",
                ReasoningEffort::Max,
                true
            ),
            Some(ReasoningEffort::Off)
        );
        // Backward from off wraps to the native top (max).
        assert_eq!(
            cycle_effort(
                ProviderId::OpenAiCodex,
                "gpt-5.6-sol",
                ReasoningEffort::Off,
                false
            ),
            Some(ReasoningEffort::Max)
        );
        // OpenAI-compatible exposes off/low/medium/high; high wraps to off.
        assert_eq!(
            cycle_effort(
                ProviderId::OpenAiCompatible,
                "gpt-test",
                ReasoningEffort::High,
                true
            ),
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
        assert!(!supports_thinking(ProviderId::OpenAi, "gpt-4.1"));
        assert!(supports_thinking(ProviderId::OpenAiCompatible, "gpt-test"));
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
        // OpenAI-compatible exposes low/medium/high: carry-over minimal/xhigh
        // clamp to the nearest native endpoint levels.
        assert_eq!(
            clamp(
                ProviderId::OpenAiCompatible,
                "gpt-test",
                ReasoningEffort::Minimal
            ),
            ReasoningEffort::Low
        );
        assert_eq!(
            clamp(
                ProviderId::OpenAiCompatible,
                "gpt-test",
                ReasoningEffort::XHigh
            ),
            ReasoningEffort::High
        );
        // Built-in OpenAI API chat models do not support reasoning_effort.
        assert_eq!(
            clamp(ProviderId::OpenAi, "gpt-4.1", ReasoningEffort::High),
            ReasoningEffort::Off
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
        // `max` is native to the GPT-5.6 family only; carrying it to an older
        // Codex model lands on its highest supported effort.
        assert_eq!(
            clamp(ProviderId::OpenAiCodex, "gpt-5.5", ReasoningEffort::Max),
            ReasoningEffort::XHigh
        );
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert_eq!(
                clamp(ProviderId::OpenAiCodex, model, ReasoningEffort::Max),
                ReasoningEffort::Max,
                "{model} keeps max"
            );
        }
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
