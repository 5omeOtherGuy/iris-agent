//! Lane descriptors: the (provider lane, model id, reasoning effort) tuples a
//! campaign cell runs against. Descriptors are data, not code paths -- a new
//! model or effort is a new `LaneSpec`, not a new hand-written test. Provider
//! construction is reused here for both lanes so the campaign runner and the
//! legacy live tests build providers the same way.
//!
//! Availability is discovered at runtime (credentials present), never at
//! compile time: the Codex/Luna descriptor is defined unconditionally so the
//! matrix is complete, and its runtime use is skipped when the Codex OAuth
//! credential is absent. This keeps the harness compilable and the pilot
//! runnable before the Luna provider lane is wired end to end.

use super::*;
use crate::mimir::auth::anthropic::claude_code_credentials_available;
use crate::mimir::auth::storage::AuthStore;
use crate::mimir::providers::anthropic_messages::AnthropicProvider;
use crate::mimir::providers::openai_codex_responses::OpenAiCodexResponsesProvider;
use crate::mimir::retry::RetryPolicy;
use crate::mimir::selection::{
    CodexTransport, ContextManagement, PromptCacheRetention, ReasoningEffort,
};

/// Minimal system prompt; each provider prepends its own required identity
/// block, so the harness supplies only a short behavioral hint.
pub(crate) const LANE_SYSTEM_PROMPT: &str = "You are a coding assistant. Keep answers short.";

/// The OAuth provider id the Codex token store is keyed under (see
/// `crate::mimir::auth::openai_codex`). Used only for availability discovery.
const CODEX_AUTH_PROVIDER: &str = "openai-codex";

/// The subscription/OAuth provider a lane runs on. Provider-specific wire facts
/// stay inside the provider modules; a lane only names which of them to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderLane {
    /// Anthropic Messages on the Claude Code subscription OAuth lane -- the only
    /// lane that reports cache writes (plus the 5m/1h split) and native
    /// compaction.
    Anthropic,
    /// OpenAI Codex Responses on the Codex subscription OAuth lane -- write-blind
    /// (no reported cache writes) and no native compaction rung.
    Codex,
}

impl ProviderLane {
    /// Accepted provider tokens, listed in every parse error.
    pub(crate) const ACCEPTED: [&'static str; 2] = ["anthropic", "codex"];

    /// Parse a provider token from a config file. Unknown tokens name the field,
    /// the offending value, and the accepted set (system-boundary validation).
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "anthropic" => Ok(Self::Anthropic),
            "codex" => Ok(Self::Codex),
            other => Err(anyhow::anyhow!(
                "lane provider {other:?} is not supported; accepted providers: {}",
                Self::ACCEPTED.join(", ")
            )),
        }
    }
}

/// The reasoning/thinking effort a lane requests. Pilots stay `Low`; `Medium`
/// and `High` are for the quality campaigns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaneEffort {
    Low,
    Medium,
    High,
}

impl LaneEffort {
    /// Accepted effort tokens, listed in every parse error so the message names
    /// the valid set (config validation is a system boundary).
    pub(crate) const ACCEPTED: [&'static str; 3] = ["low", "medium", "high"];

    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    fn reasoning(self) -> ReasoningEffort {
        match self {
            Self::Low => ReasoningEffort::Low,
            Self::Medium => ReasoningEffort::Medium,
            Self::High => ReasoningEffort::High,
        }
    }

    /// Parse an effort token from a config file, verbatim. Unknown tokens name
    /// the field, the offending value, and the accepted set.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(anyhow::anyhow!(
                "lane effort {other:?} is not supported; accepted efforts: {}",
                Self::ACCEPTED.join(", ")
            )),
        }
    }
}

/// One lane descriptor: the provider lane, the model id it drives, and the
/// effort it requests. `model_id` is an owned string so a config file can name
/// any model verbatim; the provider constructor receives it unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaneSpec {
    pub(crate) lane: ProviderLane,
    pub(crate) model_id: String,
    pub(crate) effort: LaneEffort,
}

impl LaneSpec {
    /// Generic construction from a (provider kind, model id, effort) tuple --
    /// the single path both the config loader and the const-fn fixtures build
    /// through. The model id is passed to the provider constructor verbatim.
    pub(crate) fn new(lane: ProviderLane, model_id: impl Into<String>, effort: LaneEffort) -> Self {
        Self {
            lane,
            model_id: model_id.into(),
            effort,
        }
    }

    /// A stable label used in row schemas and manifests: `provider/model@effort`.
    pub(crate) fn label(&self) -> String {
        let provider = match self.lane {
            ProviderLane::Anthropic => "anthropic",
            ProviderLane::Codex => "openai-codex",
        };
        format!("{provider}/{}@{}", self.model_id, self.effort.as_str())
    }

    /// How this lane's realized cache mass is read from `ProviderUsage`. The
    /// write-visible Anthropic lane reports writes; the write-blind Codex lane
    /// can only derive fresh input.
    pub(crate) fn cache_mass_model(&self) -> CacheMassModel {
        match self.lane {
            ProviderLane::Anthropic => CacheMassModel::ReportedWrite,
            ProviderLane::Codex => CacheMassModel::DerivedFreshInput,
        }
    }

    /// Whether this lane exposes a provider-native compaction rung. Codex has
    /// none, so a campaign cell that requires native compaction is invalid on
    /// the Codex lane.
    pub(crate) fn supports_native_compaction(&self) -> bool {
        matches!(self.lane, ProviderLane::Anthropic)
    }

    /// Whether this lane's credentials are present, so the campaign runner can
    /// skip a cell rather than fail when the operator has not authorized a lane.
    /// Discovery only -- it never triggers a network call.
    pub(crate) fn available(&self) -> bool {
        match self.lane {
            ProviderLane::Anthropic => claude_code_credentials_available(),
            ProviderLane::Codex => AuthStore::from_env()
                .and_then(|store| store.oauth_credentials(CODEX_AUTH_PROVIDER))
                .is_ok(),
        }
    }

    /// Build the real provider for this lane. `cache_key` scopes the Codex
    /// prompt cache; it is ignored by the Anthropic lane.
    pub(crate) fn build_provider(&self, cache_key: &str) -> Result<Box<dyn ChatProvider>> {
        match self.lane {
            ProviderLane::Anthropic => Ok(Box::new(AnthropicProvider::new(
                &self.model_id,
                "https://api.anthropic.com",
                Some(self.effort.reasoning()),
                LANE_SYSTEM_PROMPT,
                PromptCacheRetention::DEFAULT,
                ContextManagement::default(),
                RetryPolicy::default(),
            )?)),
            ProviderLane::Codex => Ok(Box::new(OpenAiCodexResponsesProvider::new(
                &self.model_id,
                "https://chatgpt.com/backend-api",
                Some(self.effort.reasoning()),
                LANE_SYSTEM_PROMPT,
                cache_key,
                PromptCacheRetention::DEFAULT,
                RetryPolicy::default(),
                CodexTransport::Auto,
                Some(std::time::Duration::from_secs(300)),
            )?)),
        }
    }
}

/// The initial anthropic/sonnet-4.6 lane at the given effort. A thin fixture
/// over [`LaneSpec::new`]; config campaigns build the same shape from `[[lanes]]`.
pub(crate) fn anthropic_sonnet(effort: LaneEffort) -> LaneSpec {
    LaneSpec::new(ProviderLane::Anthropic, "claude-sonnet-4-6", effort)
}

/// The initial codex/gpt-5.6-luna lane at the given effort. Defined
/// unconditionally; runtime use is gated on [`LaneSpec::available`].
pub(crate) fn codex_luna(effort: LaneEffort) -> LaneSpec {
    LaneSpec::new(ProviderLane::Codex, "gpt-5.6-luna", effort)
}

/// Every lane the harness knows about, in a stable order. A campaign selects a
/// subset by label.
pub(crate) fn initial_lanes() -> Vec<LaneSpec> {
    vec![
        anthropic_sonnet(LaneEffort::Low),
        anthropic_sonnet(LaneEffort::Medium),
        codex_luna(LaneEffort::Low),
        codex_luna(LaneEffort::Medium),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_labels_are_stable_and_distinct() {
        let labels: Vec<String> = initial_lanes().iter().map(LaneSpec::label).collect();
        assert_eq!(
            labels,
            vec![
                "anthropic/claude-sonnet-4-6@low",
                "anthropic/claude-sonnet-4-6@medium",
                "openai-codex/gpt-5.6-luna@low",
                "openai-codex/gpt-5.6-luna@medium",
            ]
        );
    }

    #[test]
    fn provider_and_effort_parse_accepts_known_and_names_the_set_on_error() {
        assert_eq!(
            ProviderLane::parse("anthropic").unwrap(),
            ProviderLane::Anthropic
        );
        assert_eq!(ProviderLane::parse("codex").unwrap(), ProviderLane::Codex);
        let err = ProviderLane::parse("gemini").unwrap_err().to_string();
        assert!(err.contains("gemini"), "names the offending value: {err}");
        assert!(
            err.contains("anthropic") && err.contains("codex"),
            "lists the set: {err}"
        );

        assert_eq!(LaneEffort::parse("high").unwrap(), LaneEffort::High);
        let err = LaneEffort::parse("turbo").unwrap_err().to_string();
        assert!(
            err.contains("turbo") && err.contains("low, medium, high"),
            "{err}"
        );
    }

    #[test]
    fn generic_construction_drives_any_model_verbatim() {
        let lane = LaneSpec::new(ProviderLane::Anthropic, "claude-opus-9", LaneEffort::High);
        assert_eq!(lane.label(), "anthropic/claude-opus-9@high");
        assert_eq!(lane.model_id, "claude-opus-9");
    }

    #[test]
    fn lane_asymmetries_match_provider_reality() {
        assert!(anthropic_sonnet(LaneEffort::Low).supports_native_compaction());
        assert!(!codex_luna(LaneEffort::Low).supports_native_compaction());
        assert_eq!(
            anthropic_sonnet(LaneEffort::Low).cache_mass_model(),
            CacheMassModel::ReportedWrite
        );
        assert_eq!(
            codex_luna(LaneEffort::Low).cache_mass_model(),
            CacheMassModel::DerivedFreshInput
        );
    }
}
