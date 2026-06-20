//! Reasoning capability validation and clamp (Tier 3 Mimir): one hard-coded
//! table that says which [`ReasoningEffort`] levels a given provider/model
//! accepts, plus the [`clamp`] that down-maps an unsupported level the way
//! pi-mono's `clampReasoning` does (`xhigh -> high`).
//!
//! Conceptually ported from pi-mono's capability filtering
//! (`getSupportedThinkingLevels`) and `clampReasoning`
//! (`packages/ai/src/providers/simple-options.ts`); the generated registry and
//! per-model compat matrices are not adopted -- the data for the three supported
//! providers is hard-coded here, keyed by provider with a per-model dimension
//! reserved for future overrides.
//!
//! Supported sets were verified against pi-mono's model registry and the
//! gemini-pi / official provider docs (see the task report, unit 10):
//! - openai-codex (gpt-5.5): full set incl. `xhigh`
//!   (`models.generated.ts` `thinkingLevelMap: {off:null, xhigh:xhigh}`; the
//!   Responses `reasoning.effort` enum accepts `minimal..xhigh`).
//! - anthropic: model-specific. The adaptive-thinking Opus 4.6/4.7/4.8 models
//!   accept `xhigh` (`models.generated.ts` `thinkingLevelMap` has an `xhigh`
//!   entry); Sonnet 4.6 and the older budget-based models top out at `high`
//!   (`xhigh` clamps to `high` via pi-mono `clampReasoning`).
//! - antigravity (gemini-3.5-flash): `off..high`; `xhigh` clamps to `high`
//!   (gemini-pi `FLASH_THINKING = {minimal,low,medium,high}`, `xhigh -> null`).

use anyhow::Result;

use crate::errors::UsageError;
use crate::mimir::selection::{ModelSelection, ProviderId, ReasoningEffort};

/// Reasoning levels a provider/model natively accepts. Codex and Antigravity
/// share one set per provider; Anthropic is model-specific (adaptive Opus models
/// add `xhigh`). Unknown/custom ids fall back to the conservative per-provider
/// set.
pub(crate) fn supported_levels(provider: ProviderId, model: &str) -> &'static [ReasoningEffort] {
    use ReasoningEffort::{High, Low, Medium, Minimal, Off, XHigh};
    match provider {
        // gpt-5.5 accepts the full effort range, including xhigh.
        ProviderId::OpenAiCodex => &[Off, Minimal, Low, Medium, High, XHigh],
        // Anthropic depends on the model: adaptive-thinking Opus 4.6/4.7/4.8
        // accept xhigh; Sonnet 4.6 and older budget models top out at high.
        ProviderId::Anthropic => anthropic_supported_levels(model),
        // gemini-3.5-flash (Flash tier) tops out at high; xhigh down-clamps.
        ProviderId::Antigravity => &[Off, Minimal, Low, Medium, High],
    }
}

/// Anthropic supported levels, keyed by model. The adaptive-thinking Opus
/// 4.6/4.7/4.8 models accept `xhigh` (the adapter maps it to the `xhigh`/`max`
/// effort token); Sonnet 4.6 and the older budget-based models top out at
/// `high`. Verified from pi-mono `thinkingLevelMap` (an `xhigh` entry means
/// xhigh is an accepted input level).
fn anthropic_supported_levels(model: &str) -> &'static [ReasoningEffort] {
    use ReasoningEffort::{High, Low, Medium, Minimal, Off, XHigh};
    match model {
        "claude-opus-4-6" | "claude-opus-4-7" | "claude-opus-4-8" => {
            &[Off, Minimal, Low, Medium, High, XHigh]
        }
        _ => &[Off, Minimal, Low, Medium, High],
    }
}

/// Validate an explicit reasoning preference against the active model. `None`
/// always passes (no preference -> today's wire). `Some(level)` errors with an
/// actionable message when the level is not natively supported, so a configured
/// `defaultReasoning` that the model rejects fails loudly at startup.
pub(crate) fn validate(selection: &ModelSelection) -> Result<()> {
    let Some(level) = selection.reasoning else {
        return Ok(());
    };
    let supported = supported_levels(selection.provider, &selection.model);
    if supported.contains(&level) {
        return Ok(());
    }
    Err(UsageError::new(format!(
        "reasoning level '{}' is not supported by {}/{}; supported: {}",
        level.as_str(),
        selection.provider.as_str(),
        selection.model,
        join_levels(supported),
    ))
    .into())
}

/// Down-map an unsupported level to the nearest representable one. Mirrors
/// pi-mono `clampReasoning`: `xhigh -> high` where `xhigh` is unsupported. Any
/// other unsupported level is returned unchanged so [`validate`] surfaces it
/// rather than silently substituting a different effort.
pub(crate) fn clamp(provider: ProviderId, model: &str, level: ReasoningEffort) -> ReasoningEffort {
    let supported = supported_levels(provider, model);
    if supported.contains(&level) {
        return level;
    }
    if level == ReasoningEffort::XHigh && supported.contains(&ReasoningEffort::High) {
        return ReasoningEffort::High;
    }
    level
}

/// Comma-join level tokens for an error/info message.
pub(crate) fn join_levels(levels: &[ReasoningEffort]) -> String {
    levels
        .iter()
        .map(|level| level.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(provider: ProviderId, reasoning: Option<ReasoningEffort>) -> ModelSelection {
        ModelSelection {
            provider,
            model: "m".to_string(),
            base_url: "https://example".to_string(),
            reasoning,
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
        // The actionable part: which levels ARE supported.
        assert!(err.contains("off, minimal, low, medium, high"), "{err}");
    }

    #[test]
    fn anthropic_xhigh_is_model_specific() {
        // Adaptive Opus 4.7/4.8 accept xhigh natively: validate passes, clamp is
        // identity.
        for model in ["claude-opus-4-7", "claude-opus-4-8", "claude-opus-4-6"] {
            let sel = ModelSelection {
                provider: ProviderId::Anthropic,
                model: model.to_string(),
                base_url: "https://example".to_string(),
                reasoning: Some(ReasoningEffort::XHigh),
            };
            assert!(validate(&sel).is_ok(), "{model} should accept xhigh");
            assert_eq!(
                clamp(ProviderId::Anthropic, model, ReasoningEffort::XHigh),
                ReasoningEffort::XHigh,
                "{model} keeps xhigh"
            );
        }
        // Sonnet 4.6 (and unknown ids) top out at high: xhigh clamps and validate
        // rejects the unclamped level.
        assert_eq!(
            clamp(
                ProviderId::Anthropic,
                "claude-sonnet-4-6",
                ReasoningEffort::XHigh
            ),
            ReasoningEffort::High
        );
    }

    #[test]
    fn clamp_down_maps_xhigh_to_high_only_where_unsupported() {
        // Anthropic/Antigravity: xhigh -> high.
        assert_eq!(
            clamp(
                ProviderId::Anthropic,
                "claude-sonnet-4-6",
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
