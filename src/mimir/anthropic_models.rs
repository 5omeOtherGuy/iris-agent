//! Anthropic Claude Code subscription model metadata (Tier 3 Mimir): the single
//! source of truth for the subscription model matrix's wire facts -- model id,
//! output-token cap, thinking mode, and refusal fallback. Adopted from
//! minimalcc-pi's `src/models.ts` `MODELS` table (the Claude Code subscription
//! lane), implemented in Iris's Rust selection layer.
//!
//! Ownership is split so each fact has one home:
//! - wire facts (model id, output cap, thinking mode, fallback) live here and
//!   drive `providers::anthropic_messages` request construction;
//! - display facts (picker name, context-window label) live in `model_catalog`;
//! - supported reasoning levels are derived from [`ThinkingMode`] /
//!   subscription-model membership in `model_capabilities`.
//!
//! Iris deliberately does not adopt minimalcc-pi's Pi extension architecture,
//! provider-registration APIs, or TypeScript module layout (see the task
//! report): only the model matrix's wire behavior is ported.

/// How a subscription model encodes reasoning on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingMode {
    /// Manual budget: `thinking: { type: "enabled", budget_tokens: N }` with the
    /// `max_tokens` invariant (`min(requested_output + budget, output_cap)`,
    /// `1024 <= budget_tokens < max_tokens`). Haiku 4.5, Sonnet 4.6, Opus 4.6.
    ManualBudget,
    /// Adaptive: `thinking: { type: "adaptive", display: "summarized" }` plus
    /// `output_config.effort`; the API allocates reasoning dynamically. Sonnet
    /// 5, Opus 4.7/4.8, and Fable 5.
    Adaptive,
}

/// One subscription model's wire facts.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AnthropicModel {
    /// Catalog/UI id (what `/model` shows and selection stores). This is also the
    /// upstream model id sent in the request body.
    pub(crate) ui_id: &'static str,
    /// Per-model output-token cap (the synchronous `/v1/messages` ceiling). Used
    /// as the upper bound of the manual-budget `max_tokens` invariant.
    pub(crate) output_cap: u32,
    /// Thinking encoding for this model.
    pub(crate) thinking: ThinkingMode,
    /// Native model the API retries server-side on a safety refusal (Fable 5
    /// `stop_reason: "refusal"`), sent as the `fallbacks` request parameter under
    /// the server-side fallback beta. `None` for every other model.
    pub(crate) refusal_fallback: Option<&'static str>,
}

use ThinkingMode::{Adaptive, ManualBudget};

/// The Claude Code subscription model matrix. Mirrors minimalcc-pi `MODELS`,
/// less the soft-cap aliases: every entry's UI id is the upstream model id.
pub(crate) const MODELS: &[AnthropicModel] = &[
    AnthropicModel {
        ui_id: "claude-haiku-4-5",
        output_cap: 64000,
        thinking: ManualBudget,
        refusal_fallback: None,
    },
    AnthropicModel {
        ui_id: "claude-sonnet-5",
        output_cap: 128000,
        thinking: Adaptive,
        refusal_fallback: None,
    },
    AnthropicModel {
        ui_id: "claude-sonnet-4-6",
        output_cap: 64000,
        thinking: ManualBudget,
        refusal_fallback: None,
    },
    AnthropicModel {
        ui_id: "claude-opus-4-6",
        output_cap: 128000,
        thinking: ManualBudget,
        refusal_fallback: None,
    },
    AnthropicModel {
        ui_id: "claude-opus-4-7",
        output_cap: 128000,
        thinking: Adaptive,
        refusal_fallback: None,
    },
    AnthropicModel {
        ui_id: "claude-opus-4-8",
        output_cap: 128000,
        thinking: Adaptive,
        refusal_fallback: None,
    },
    AnthropicModel {
        ui_id: "claude-fable-5",
        output_cap: 128000,
        thinking: Adaptive,
        refusal_fallback: Some("claude-opus-4-8"),
    },
];

/// Look up a subscription model by its UI id. `None` for any id not in the
/// matrix (older/unknown Anthropic ids), which callers treat conservatively.
pub(crate) fn find(ui_id: &str) -> Option<&'static AnthropicModel> {
    MODELS.iter().find(|model| model.ui_id == ui_id)
}

/// Whether `ui_id` is a known Claude Code subscription model.
pub(crate) fn is_subscription_model(ui_id: &str) -> bool {
    find(ui_id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_modes_and_caps_match_the_matrix() {
        let by_id = |id: &str| find(id).expect(id);
        // Manual-budget tier.
        for id in ["claude-haiku-4-5", "claude-sonnet-4-6", "claude-opus-4-6"] {
            assert_eq!(by_id(id).thinking, ManualBudget, "{id}");
        }
        // Adaptive tier.
        for id in [
            "claude-sonnet-5",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-fable-5",
        ] {
            assert_eq!(by_id(id).thinking, Adaptive, "{id}");
        }
        // Output caps: 64k for Haiku/Sonnet 4.6, 128k for the Sonnet 5/Opus/Fable tier.
        assert_eq!(by_id("claude-haiku-4-5").output_cap, 64000);
        assert_eq!(by_id("claude-sonnet-4-6").output_cap, 64000);
        assert_eq!(by_id("claude-sonnet-5").output_cap, 128000);
        assert_eq!(by_id("claude-opus-4-6").output_cap, 128000);
        assert_eq!(by_id("claude-opus-4-8").output_cap, 128000);
    }

    #[test]
    fn only_fable_5_carries_a_refusal_fallback() {
        for model in MODELS {
            if model.ui_id == "claude-fable-5" {
                assert_eq!(model.refusal_fallback, Some("claude-opus-4-8"));
            } else {
                assert_eq!(model.refusal_fallback, None, "{}", model.ui_id);
            }
        }
    }

    #[test]
    fn find_and_membership_agree() {
        assert!(is_subscription_model("claude-opus-4-7"));
        assert!(find("claude-3-7-sonnet").is_none());
        assert!(!is_subscription_model("claude-3-7-sonnet"));
        // The retired 300k soft-cap alias is no longer a known model.
        assert!(!is_subscription_model("claude-opus-4-7-300k"));
    }
}
