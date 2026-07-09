//! Typed tool-result compaction settings and provider-neutral local policy.

use anyhow::{Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolResultCompactionSettings {
    pub(crate) enabled: Option<bool>,
    pub(crate) aggressiveness: Option<String>,
    pub(crate) cache_timing: Option<String>,
    pub(crate) trigger_tokens: Option<u64>,
    pub(crate) semantic_dedupe: Option<SemanticDedupeSettings>,
    pub(crate) tool_clearing: Option<ToolClearingSettings>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticDedupeSettings {
    pub(crate) enabled: Option<bool>,
    pub(crate) retain_per_path: Option<u64>,
    pub(crate) protect_recent_tool_results: Option<u64>,
    pub(crate) protect_recent_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolClearingSettings {
    pub(crate) enabled: Option<bool>,
    pub(crate) backend: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) keep_recent_tool_uses: Option<u64>,
    pub(crate) clear_at_least_tokens: Option<u64>,
    pub(crate) eligible_tools: Option<Vec<String>>,
    pub(crate) excluded_tools: Option<Vec<String>>,
    pub(crate) include_failures: Option<bool>,
    pub(crate) clear_tool_inputs: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionAggressiveness {
    Conservative,
    Balanced,
    Aggressive,
    Custom,
}

impl CompactionAggressiveness {
    pub(super) fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim).unwrap_or("conservative") {
            "conservative" => Ok(Self::Conservative),
            "balanced" => Ok(Self::Balanced),
            "aggressive" => Ok(Self::Aggressive),
            "custom" => Ok(Self::Custom),
            value => bail!(
                "invalid toolResultCompaction.aggressiveness {value:?}; expected conservative, balanced, aggressive, or custom"
            ),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Conservative => "conservative",
            Self::Balanced => "balanced",
            Self::Aggressive => "aggressive",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionCacheTiming {
    BreakOnly,
    CacheAware,
    PressureOnly,
    Immediate,
}

impl CompactionCacheTiming {
    pub(super) fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim).unwrap_or("cacheAware") {
            "breakOnly" => Ok(Self::BreakOnly),
            "cacheAware" => Ok(Self::CacheAware),
            "pressureOnly" => Ok(Self::PressureOnly),
            "immediate" => Ok(Self::Immediate),
            value => bail!(
                "invalid toolResultCompaction.cacheTiming {value:?}; expected breakOnly, cacheAware, pressureOnly, or immediate"
            ),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BreakOnly => "breakOnly",
            Self::CacheAware => "cacheAware",
            Self::PressureOnly => "pressureOnly",
            Self::Immediate => "immediate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolClearingBackend {
    Local,
    AnthropicNative,
    Auto,
}

impl ToolClearingBackend {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim).unwrap_or("local") {
            "local" => Ok(Self::Local),
            "anthropicNative" => Ok(Self::AnthropicNative),
            "auto" => Ok(Self::Auto),
            value => bail!(
                "invalid toolResultCompaction.toolClearing.backend {value:?}; expected local, anthropicNative, or auto"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolClearingMode {
    Replayable,
    Selected,
    AllRecoverable,
}

impl ToolClearingMode {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim).unwrap_or("replayable") {
            "replayable" => Ok(Self::Replayable),
            "selected" => Ok(Self::Selected),
            "allRecoverable" => Ok(Self::AllRecoverable),
            value => bail!(
                "invalid toolResultCompaction.toolClearing.mode {value:?}; expected replayable, selected, or allRecoverable"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticDedupePolicy {
    pub(crate) enabled: bool,
    pub(crate) retain_per_path: u64,
    pub(crate) protect_recent_tool_results: u64,
    pub(crate) protect_recent_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolClearingPolicy {
    pub(crate) enabled: bool,
    pub(crate) backend: ToolClearingBackend,
    pub(crate) mode: ToolClearingMode,
    pub(crate) keep_recent_tool_uses: u64,
    pub(crate) clear_at_least_tokens: u64,
    pub(crate) eligible_tools: Vec<String>,
    pub(crate) excluded_tools: Vec<String>,
    pub(crate) include_failures: bool,
    pub(crate) clear_tool_inputs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResultCompactionPolicy {
    pub(crate) enabled: bool,
    pub(crate) aggressiveness: CompactionAggressiveness,
    pub(crate) cache_timing: CompactionCacheTiming,
    pub(crate) trigger_tokens: u64,
    pub(crate) semantic_dedupe: SemanticDedupePolicy,
    pub(crate) tool_clearing: ToolClearingPolicy,
    pub(crate) legacy_alias: bool,
}

pub(super) fn merge(
    global: Option<ToolResultCompactionSettings>,
    project: Option<ToolResultCompactionSettings>,
) -> Option<ToolResultCompactionSettings> {
    let Some(project) = project else {
        return global;
    };
    let global_native = native_backend(global.as_ref());
    let project_requests_native = native_backend(Some(&project));
    let mut merged = global.unwrap_or_default();
    if global_native {
        merged.cache_timing = project.cache_timing.or(merged.cache_timing);
        merged.trigger_tokens = project.trigger_tokens.or(merged.trigger_tokens);
        merged.semantic_dedupe = merge_semantic(merged.semantic_dedupe, project.semantic_dedupe);
        return Some(merged);
    }
    merged.enabled = project.enabled.or(merged.enabled);
    merged.aggressiveness = project.aggressiveness.or(merged.aggressiveness);
    merged.cache_timing = project.cache_timing.or(merged.cache_timing);
    merged.trigger_tokens = project.trigger_tokens.or(merged.trigger_tokens);
    merged.semantic_dedupe = merge_semantic(merged.semantic_dedupe, project.semantic_dedupe);
    if !project_requests_native {
        merged.tool_clearing = merge_clearing(merged.tool_clearing, project.tool_clearing);
    }
    Some(merged)
}

fn native_backend(settings: Option<&ToolResultCompactionSettings>) -> bool {
    settings
        .and_then(|settings| settings.tool_clearing.as_ref())
        .and_then(|clearing| clearing.backend.as_deref())
        .is_some_and(|backend| matches!(backend.trim(), "anthropicNative" | "auto"))
}

fn merge_semantic(
    global: Option<SemanticDedupeSettings>,
    project: Option<SemanticDedupeSettings>,
) -> Option<SemanticDedupeSettings> {
    let Some(project) = project else {
        return global;
    };
    let mut merged = global.unwrap_or_default();
    merged.enabled = project.enabled.or(merged.enabled);
    merged.retain_per_path = project.retain_per_path.or(merged.retain_per_path);
    merged.protect_recent_tool_results = project
        .protect_recent_tool_results
        .or(merged.protect_recent_tool_results);
    merged.protect_recent_tokens = project
        .protect_recent_tokens
        .or(merged.protect_recent_tokens);
    Some(merged)
}

fn merge_clearing(
    global: Option<ToolClearingSettings>,
    project: Option<ToolClearingSettings>,
) -> Option<ToolClearingSettings> {
    let Some(project) = project else {
        return global;
    };
    let mut merged = global.unwrap_or_default();
    merged.enabled = project.enabled.or(merged.enabled);
    merged.backend = project.backend.or(merged.backend);
    merged.mode = project.mode.or(merged.mode);
    merged.keep_recent_tool_uses = project
        .keep_recent_tool_uses
        .or(merged.keep_recent_tool_uses);
    merged.clear_at_least_tokens = project
        .clear_at_least_tokens
        .or(merged.clear_at_least_tokens);
    merged.eligible_tools = project.eligible_tools.or(merged.eligible_tools);
    merged.excluded_tools = project.excluded_tools.or(merged.excluded_tools);
    merged.include_failures = project.include_failures.or(merged.include_failures);
    merged.clear_tool_inputs = project.clear_tool_inputs.or(merged.clear_tool_inputs);
    Some(merged)
}

pub(super) fn resolve(
    raw: Option<&ToolResultCompactionSettings>,
    legacy_enabled: bool,
    legacy_trigger_tokens: u64,
) -> Result<ToolResultCompactionPolicy> {
    let legacy_alias = raw.is_none();
    let aggressiveness = CompactionAggressiveness::parse(
        raw.and_then(|settings| settings.aggressiveness.as_deref()),
    )?;
    let enabled = raw
        .and_then(|settings| settings.enabled)
        .unwrap_or(legacy_alias && legacy_enabled);
    let cache_timing =
        CompactionCacheTiming::parse(raw.and_then(|settings| settings.cache_timing.as_deref()))?;
    let trigger_tokens = raw
        .and_then(|settings| settings.trigger_tokens)
        .unwrap_or(legacy_trigger_tokens);
    if trigger_tokens == 0 {
        bail!("toolResultCompaction.triggerTokens must be greater than zero");
    }

    let (mut semantic_dedupe, mut tool_clearing) = preset(aggressiveness);
    if let Some(semantic) = raw.and_then(|settings| settings.semantic_dedupe.as_ref()) {
        semantic_dedupe.enabled = semantic.enabled.unwrap_or(semantic_dedupe.enabled);
        semantic_dedupe.retain_per_path = semantic
            .retain_per_path
            .unwrap_or(semantic_dedupe.retain_per_path);
        semantic_dedupe.protect_recent_tool_results = semantic
            .protect_recent_tool_results
            .unwrap_or(semantic_dedupe.protect_recent_tool_results);
        semantic_dedupe.protect_recent_tokens = semantic
            .protect_recent_tokens
            .unwrap_or(semantic_dedupe.protect_recent_tokens);
    }
    if semantic_dedupe.retain_per_path == 0 {
        bail!("toolResultCompaction.semanticDedupe.retainPerPath must be at least 1");
    }

    if let Some(clearing) = raw.and_then(|settings| settings.tool_clearing.as_ref()) {
        tool_clearing.enabled = clearing.enabled.unwrap_or(tool_clearing.enabled);
        tool_clearing.backend = ToolClearingBackend::parse(clearing.backend.as_deref())?;
        tool_clearing.mode = ToolClearingMode::parse(clearing.mode.as_deref())?;
        tool_clearing.keep_recent_tool_uses = clearing
            .keep_recent_tool_uses
            .unwrap_or(tool_clearing.keep_recent_tool_uses);
        tool_clearing.clear_at_least_tokens = clearing
            .clear_at_least_tokens
            .unwrap_or(tool_clearing.clear_at_least_tokens);
        tool_clearing.eligible_tools = normalize_names(
            "toolResultCompaction.toolClearing.eligibleTools",
            clearing
                .eligible_tools
                .clone()
                .unwrap_or(tool_clearing.eligible_tools),
        )?;
        tool_clearing.excluded_tools = normalize_names(
            "toolResultCompaction.toolClearing.excludedTools",
            clearing
                .excluded_tools
                .clone()
                .unwrap_or(tool_clearing.excluded_tools),
        )?;
        tool_clearing.include_failures = clearing
            .include_failures
            .unwrap_or(tool_clearing.include_failures);
        tool_clearing.clear_tool_inputs = clearing
            .clear_tool_inputs
            .unwrap_or(tool_clearing.clear_tool_inputs);
    }
    validate(enabled, &semantic_dedupe, &tool_clearing)?;
    Ok(ToolResultCompactionPolicy {
        enabled,
        aggressiveness,
        cache_timing,
        trigger_tokens,
        semantic_dedupe,
        tool_clearing,
        legacy_alias,
    })
}

fn validate(
    enabled: bool,
    semantic: &SemanticDedupePolicy,
    clearing: &ToolClearingPolicy,
) -> Result<()> {
    if clearing.enabled {
        if clearing.keep_recent_tool_uses == 0 {
            bail!("toolResultCompaction.toolClearing.keepRecentToolUses must be at least 1");
        }
        if clearing.clear_at_least_tokens == 0 {
            bail!("toolResultCompaction.toolClearing.clearAtLeastTokens must be greater than zero");
        }
        if clearing.mode == ToolClearingMode::Selected && clearing.eligible_tools.is_empty() {
            bail!(
                "toolResultCompaction.toolClearing.eligibleTools cannot be empty when mode is selected"
            );
        }
    }
    let local_reducer = semantic.enabled
        || (clearing.enabled
            && matches!(
                clearing.backend,
                ToolClearingBackend::Local | ToolClearingBackend::Auto
            ));
    if enabled
        && local_reducer
        && semantic.protect_recent_tool_results == 0
        && semantic.protect_recent_tokens == 0
    {
        bail!(
            "toolResultCompaction must protect a recent working set: set semanticDedupe.protectRecentToolResults or protectRecentTokens above zero"
        );
    }
    Ok(())
}

fn preset(aggressiveness: CompactionAggressiveness) -> (SemanticDedupePolicy, ToolClearingPolicy) {
    let semantic_enabled = aggressiveness != CompactionAggressiveness::Custom;
    let (clearing_enabled, mode, keep_recent_tool_uses) = match aggressiveness {
        CompactionAggressiveness::Conservative | CompactionAggressiveness::Custom => {
            (false, ToolClearingMode::Replayable, 8)
        }
        CompactionAggressiveness::Balanced => (true, ToolClearingMode::Replayable, 8),
        CompactionAggressiveness::Aggressive => (true, ToolClearingMode::AllRecoverable, 4),
    };
    (
        SemanticDedupePolicy {
            enabled: semantic_enabled,
            retain_per_path: 1,
            protect_recent_tool_results: match aggressiveness {
                CompactionAggressiveness::Conservative => 0,
                _ => 4,
            },
            protect_recent_tokens: 2_000,
        },
        ToolClearingPolicy {
            enabled: clearing_enabled,
            backend: ToolClearingBackend::Local,
            mode,
            keep_recent_tool_uses,
            clear_at_least_tokens: 1_000,
            eligible_tools: Vec::new(),
            excluded_tools: ["edit", "write", "recall", "read_output"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            include_failures: false,
            clear_tool_inputs: false,
        },
    )
}

fn normalize_names(label: &str, names: Vec<String>) -> Result<Vec<String>> {
    let mut normalized = Vec::with_capacity(names.len());
    for name in names {
        let name = name.trim();
        if name.is_empty() {
            bail!("{label} cannot contain an empty tool name");
        }
        if !normalized.iter().any(|existing| existing == name) {
            normalized.push(name.to_string());
        }
    }
    Ok(normalized)
}
