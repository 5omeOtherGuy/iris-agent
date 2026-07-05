//! Iris settings file: a focused JSON config for provider/model defaults.
//!
//! Mirrors pi's settings model (`~/.pi/agent/settings.json` +
//! `.pi/settings.json`) with one security caveat: untrusted project-local config
//! may override the model only, while provider and base-url selection come from
//! global/user config. Iris keeps its config under the same `~/.iris` directory
//! as the auth file:
//!
//! | Location                  | Scope                       |
//! | ------------------------- | --------------------------- |
//! | `~/.iris/settings.json`   | Global (all projects)       |
//! | `<cwd>/.iris/settings.json` | Project (current directory) |
//!
//! Project settings override the model only. Provider/base-url are intentionally
//! user-global so a cloned repository cannot redirect OAuth bearer tokens to a
//! malicious endpoint. Explicit runtime input still wins over the file where a
//! provider supports env overrides. Unknown keys are ignored so older binaries
//! tolerate newer config. A malformed file is a hard error -- a silently-ignored
//! config is a footgun.
//!
//! Live tool/approval policy is not configured here: the session approval mode
//! (`/approval`) and project permission grants (`/trust`) are session/project
//! state, not settings keys. The one exception is `defaultApproval`, the
//! startup approval posture, which is GLOBAL-ONLY (a cloned project must never
//! be able to weaken it -- see `merged_with`). Cross-session approval-grant
//! persistence is still tracked separately (roadmap #14).

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

/// Settings loaded from the JSON config files. Every field is optional; an
/// absent field falls back to the next layer (safe project fields -> global ->
/// built-in default, with env applied above where the provider supports it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Settings {
    /// Provider id: `openai-codex` (default), `anthropic`, or `antigravity`.
    /// Validated by the caller (`build_provider`) so an unsupported value fails
    /// loudly rather than silently.
    pub(crate) default_provider: Option<String>,
    /// Model id passed to the active provider.
    pub(crate) default_model: Option<String>,
    /// Base URL override for the active provider's API endpoint.
    pub(crate) base_url: Option<String>,
    /// Context token budget threshold. The Tier-2 harness reads it to decide
    /// when to auto-compact: when the rebuilt/current context token total
    /// exceeds this, the harness compacts at a safe turn boundary. Absent ->
    /// [`Settings::context_token_budget`] default.
    pub(crate) context_token_budget: Option<u64>,
    /// Default reasoning/thinking effort (`off|minimal|low|medium|high|xhigh`),
    /// parsed into a normalized level by `mimir::selection`. Absent -> no
    /// preference, so adapters omit all reasoning fields (today's wire). Not a
    /// security-sensitive redirect, so a project file may tune it (like
    /// [`Settings::context_token_budget`]).
    pub(crate) default_reasoning: Option<String>,
    /// Prompt cache retention (`none|short|long`). Global-only because a cloned
    /// project must not silently change provider-side prompt-cache behavior.
    /// Parsed by `mimir::selection`; absent -> the selection-layer default
    /// (`short`).
    pub(crate) prompt_cache_retention: Option<String>,
    /// Anthropic server-side context-management opt-in
    /// (`context_management.edits`). Stored as a raw JSON object and parsed into
    /// typed edits by `mimir::selection`; absent/empty -> disabled (no
    /// `context_management` field, no extra betas). Global-only: server-side
    /// context edits change request behavior and cost, so an untrusted project
    /// file must not enable them.
    pub(crate) anthropic_context_management: Option<Value>,
    /// Ordered `provider/model` ids that scope Ctrl+P cycling (the persisted
    /// `/scoped-models` selection). Like provider/base-url this controls which
    /// providers a session talks to, so it is global-only: a cloned repo cannot
    /// silently change the cycle scope.
    pub(crate) enabled_models: Option<Vec<String>>,
    /// How compaction produces its summary text: `provider` (default) asks the
    /// active model for a structured handoff summary, falling back to the
    /// deterministic excerpts on failure; `excerpts` keeps the deterministic
    /// stand-in only. A cost/quality knob like
    /// [`Settings::context_token_budget`] (it can only choose who writes the
    /// summary, never where requests go), so a project file may tune it.
    pub(crate) compaction_summarizer: Option<String>,
    /// Opt-in microcompaction (ADR-0048, #378): fold spent tool results
    /// (superseded reads, latest-read-wins) to deterministic recoverable stubs
    /// at a micro-watermark below the compaction budget. Absent/false -> off (no
    /// folds are written). A cost/quality knob like `compactionSummarizer`: it
    /// only trades in-context detail for workspace-recoverable detail and can
    /// never redirect requests, so a project file may tune it. Gates fold
    /// WRITING only; a persisted fold always rebuilds regardless of this value.
    pub(crate) microcompaction: Option<bool>,
    /// Optional graceful soft cap on tool round-trips per turn. Absent (the
    /// default) leaves the agent loop unbounded: it runs while the model emits
    /// tool calls and stops naturally, with cancellation as the runaway guard.
    /// When set, the loop ends the turn with a Notice after this many
    /// round-trips. Not a security-sensitive redirect (unlike provider/base-url),
    /// and the built-in default is already unbounded, so a project override
    /// cannot make a run more permissive than the default -- it can only impose
    /// (or raise/lower) a local loop bound. Project-tunable like
    /// [`Settings::context_token_budget`].
    pub(crate) max_tool_roundtrips: Option<usize>,
    /// Provider retry/backoff tuning (max retries, base/max backoff). Absent
    /// subfields fall back to the built-in defaults via
    /// [`Settings::retry_settings`]. Global-only: retry volume affects provider
    /// request load and cost, so an untrusted project file must not crank it up
    /// (same reasoning as `prompt_cache_retention`).
    pub(crate) retry: Option<RetrySettings>,
    /// Generic OpenAI-compatible model metadata. The provider/model/base-url are
    /// still resolved through the existing top-level defaults; this object holds
    /// capability/display flags for the configured custom endpoint.
    pub(crate) open_ai_compatible: Option<OpenAiCompatibleSettings>,
    /// Post-change verification command config (issue #265). Project-safe (a
    /// project may set it) because a verification command is model-independent,
    /// user-authored, and still runs as a NORMAL shell execution under the
    /// unchanged approval gate: bash opts out of persistent allow-always
    /// (ADR-0010), so it re-prompts each run. A cloned repo therefore cannot use
    /// it to widen permissions or redirect anything -- unlike provider/base-url,
    /// it grants no new capability, so project override is safe here. The mere
    /// presence of this block engages the feature; an absent block leaves the
    /// feature off (no post-change checks, no reporting).
    pub(crate) verify: Option<VerifySettings>,
    /// Terminal-UI behavior (ADR-0029 screen-mode policy). Display-only
    /// preferences: no security-sensitive capability lives here.
    pub(crate) tui: Option<TuiSettings>,
    /// Startup approval posture (`strict|auto|never`, ADR-0032). Parsed by
    /// [`crate::nexus::ApprovalMode::parse`] and applied to the harness at
    /// startup; an absent/invalid value leaves today's default (`strict`).
    /// GLOBAL-ONLY: a cloned project must never be able to lower the initial
    /// posture to `never`, so (like `prompt_cache_retention`) it is taken from
    /// global config and never from an untrusted project file. The live
    /// `/approval` command stays session-only and is unaffected.
    pub(crate) default_approval: Option<String>,
    /// Where the git dropdown's `w` (new worktree) gesture creates worktrees,
    /// relative to the main worktree root when not absolute. Absent ->
    /// `../wt`. Project-tunable: it only picks a local directory for a
    /// user-confirmed `git worktree add` (the resolved path is always shown
    /// before create), granting no new capability.
    pub(crate) worktree_root: Option<String>,
}

/// Terminal-UI settings block (`"tui": { ... }` in settings.json).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TuiSettings {
    /// Alt-screen pager policy: `"auto" | "always" | "never"` (ADR-0029).
    /// Parsed by `ui::screen_mode`; an invalid value is reported and the
    /// built-in default applies.
    pub(crate) alt_screen: Option<String>,
    /// Mouse-wheel scroll step in lines for the pager (default 3, clamped to
    /// `[1, 100]`).
    pub(crate) scroll_speed: Option<u16>,
    /// Freeze the working-indicator animation (accessibility). Promotes the
    /// `IRIS_REDUCED_MOTION` env switch to persisted config; the env flag still
    /// wins. Display-only preference, so a project may set it.
    pub(crate) reduced_motion: Option<bool>,
    /// Color theme id (ADR-0042). Adaptive `terminal` default; an invalid id
    /// falls back to that default.
    pub(crate) theme: Option<String>,
}

/// Raw per-project verification config (issue #265). Both fields optional: a
/// `verify` block with no `command` engages the feature but reports
/// skipped-unconfigured, and an absent `maxAttempts` falls back to the built-in
/// default. Resolved into a [`VerificationConfig`] by [`Settings::verification`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VerifySettings {
    /// Shell command run after a task's changes to check the project.
    pub(crate) command: Option<String>,
    /// Maximum verification runs before giving up (clamped to a sane cap).
    pub(crate) max_attempts: Option<u32>,
}

/// Resolved verification config the harness runs against (issue #265). `command`
/// is `None` when the `verify` block is present but sets no (non-empty) command,
/// which the loop reports as skipped-unconfigured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationConfig {
    pub(crate) command: Option<String>,
    pub(crate) max_attempts: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenAiCompatibleSettings {
    /// Context-window size in tokens for the configured custom model.
    pub(crate) context_window: Option<u64>,
    /// Whether Iris may send OpenAI-style `reasoning_effort` for this endpoint.
    pub(crate) reasoning: Option<bool>,
    /// Whether an API key is required before the model is offered. Local servers
    /// such as Ollama leave this false/absent and run with no Authorization
    /// header.
    pub(crate) api_key_required: Option<bool>,
}

/// Raw provider retry/backoff config (all fields optional). Resolved into the
/// shared `mimir` retry policy by [`Settings::retry_settings`] +
/// [`crate::mimir::selection::ModelSelection::resolve`], which fills any absent
/// subfield with the built-in default. Kept as plain config data here so the
/// settings layer does not depend on the provider transport.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RetrySettings {
    /// Maximum transient retries before giving up.
    pub(crate) max_retries: Option<u32>,
    /// Base backoff in milliseconds, doubled per retry.
    pub(crate) base_delay_ms: Option<u64>,
    /// Backoff ceiling in milliseconds.
    pub(crate) max_delay_ms: Option<u64>,
}

/// Default context token budget when none is configured. A conservative ceiling
/// that fits common model context windows; it is only surfaced through
/// [`Settings::context_token_budget`] and triggers nothing yet.
const DEFAULT_CONTEXT_TOKEN_BUDGET: u64 = 128_000;

/// Default verification attempts when a `verify` block sets no `maxAttempts`.
/// One initial run plus a couple of fix-and-retry rounds is enough to catch and
/// correct a straightforward failure without a long retry chain.
const DEFAULT_VERIFY_MAX_ATTEMPTS: u32 = 3;
/// Hard ceiling on verification attempts, so a project file cannot request an
/// unbounded retry chain of effectful shell runs.
const MAX_VERIFY_MAX_ATTEMPTS: u32 = 10;

impl Settings {
    /// Load and merge the global and project settings files for `cwd`.
    pub(crate) fn load(cwd: &Path) -> Result<Self> {
        Self::load_from(global_path().as_deref(), &project_path(cwd))
    }

    /// Core loader, split out so tests can supply explicit file paths. A
    /// missing file (or absent global path) contributes nothing; a
    /// present-but-malformed file errors.
    fn load_from(global: Option<&Path>, project: &Path) -> Result<Self> {
        let global = match global {
            Some(path) => read_optional(path)?.unwrap_or_default(),
            None => Settings::default(),
        };
        let project = read_optional(project)?.unwrap_or_default();
        Ok(global.merged_with(project))
    }

    /// Merge project settings into global settings. Project config is usually
    /// repo-controlled, so only model choice is trusted there; provider and
    /// base-url control where bearer tokens are sent and must come from global
    /// user config or built-in defaults.
    fn merged_with(self, project: Settings) -> Settings {
        Settings {
            default_provider: self.default_provider,
            default_model: project.default_model.or(self.default_model),
            base_url: self.base_url,
            // A budget is not a security-sensitive redirect (unlike provider /
            // base-url), so a project may tune it; fall back to global, then the
            // built-in default via the accessor.
            context_token_budget: project.context_token_budget.or(self.context_token_budget),
            // Reasoning effort is likewise not a security redirect, so a project
            // may override it; fall back to global.
            default_reasoning: project.default_reasoning.or(self.default_reasoning),
            // Summarizer choice is a cost/quality knob like the budget (it can
            // only pick who writes the summary text), so a project may tune it.
            compaction_summarizer: project.compaction_summarizer.or(self.compaction_summarizer),
            // Microcompaction is a cost/quality knob like the summarizer (it only
            // trades in-context detail for recoverable detail, never redirects a
            // request), so a project may tune it; project value wins, else global.
            microcompaction: project.microcompaction.or(self.microcompaction),
            // Prompt cache retention can affect privacy/cost, so keep it
            // global-only like provider/base-url and scoped model sets.
            prompt_cache_retention: self.prompt_cache_retention,
            // Context management changes request behavior/cost server-side, so
            // it is likewise global-only and never taken from project config.
            anthropic_context_management: self.anthropic_context_management,
            // Scoped models gate which providers a session cycles through, so
            // (like provider/base-url) they are global-only and never taken from
            // untrusted project config.
            enabled_models: self.enabled_models,
            // A turn cap is not a security-sensitive redirect and the default is
            // already unbounded, so a project override cannot make a run more
            // permissive than the default; project value wins, else global.
            max_tool_roundtrips: project.max_tool_roundtrips.or(self.max_tool_roundtrips),
            // Retry tuning affects provider load/cost, so keep it global-only
            // like prompt cache retention; never taken from project config.
            retry: self.retry,
            // Custom endpoint capability flags are global-only alongside the
            // base URL, so a cloned project cannot change how a secret-bearing
            // endpoint is called.
            open_ai_compatible: self.open_ai_compatible,
            // A verification command grants no new capability (it runs under the
            // unchanged approval gate; bash re-prompts every run per ADR-0010),
            // so a cloned project may set it like the model or round-trip cap;
            // project value wins, else global.
            verify: project.verify.or(self.verify),
            // Screen-mode policy is a display preference, not a security
            // redirect, so a project may set it; project value wins, else
            // global.
            tui: project.tui.or(self.tui),
            // Startup approval posture gates whether tools auto-run without a
            // prompt, so (like prompt_cache_retention) it is GLOBAL-ONLY: a
            // cloned project must never lower the initial posture to `never`.
            default_approval: self.default_approval,
            // A local worktree location preference; project value wins.
            worktree_root: project.worktree_root.or(self.worktree_root),
        }
    }

    /// Where new worktrees are created: the configured `worktreeRoot` (absolute
    /// or relative to `main_root`), defaulting to `../wt` beside the main
    /// worktree root.
    pub(crate) fn worktree_root(&self, main_root: &Path) -> PathBuf {
        let raw = self.worktree_root.as_deref().unwrap_or("../wt");
        let path = Path::new(raw);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            main_root.join(path)
        };
        // Resolve `.`/`..` lexically so the previewed and created worktree path
        // is clean (`/repos/wt/x`, not `/repos/main/../wt/x`).
        crate::tools::path::lexical_normalize(&joined)
    }

    /// The `tui` settings block, if configured.
    pub(crate) fn tui_settings(&self) -> Option<&TuiSettings> {
        self.tui.as_ref()
    }

    /// Resolved verification config, or `None` when no `verify` block is present
    /// (feature off). A present block engages the feature even with no command
    /// (reported as skipped-unconfigured). `max_attempts` is clamped to
    /// `[1, MAX_VERIFY_MAX_ATTEMPTS]` so a project file cannot request an
    /// unbounded or zero-run chain.
    pub(crate) fn verification(&self) -> Option<VerificationConfig> {
        self.verify.as_ref().map(|verify| VerificationConfig {
            command: verify
                .command
                .as_deref()
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .map(str::to_owned),
            max_attempts: verify
                .max_attempts
                .unwrap_or(DEFAULT_VERIFY_MAX_ATTEMPTS)
                .clamp(1, MAX_VERIFY_MAX_ATTEMPTS),
        })
    }

    /// Configured tool round-trip soft cap, or `None` (unbounded) when unset.
    /// The host installs it on the agent so a reached cap ends the turn with a
    /// graceful Notice instead of a fatal error.
    pub(crate) fn max_tool_roundtrips(&self) -> Option<usize> {
        self.max_tool_roundtrips
    }

    /// Raw retry settings (or an all-default empty set when unconfigured). The
    /// selection layer resolves these into the shared retry policy, filling any
    /// absent subfield with the built-in default.
    pub(crate) fn retry_settings(&self) -> RetrySettings {
        self.retry.clone().unwrap_or_default()
    }

    /// Configured context token budget, or the built-in default when unset. The
    /// harness compares the current context token total against this and
    /// auto-compacts when it is exceeded.
    pub(crate) fn context_token_budget(&self) -> u64 {
        self.context_token_budget
            .unwrap_or(DEFAULT_CONTEXT_TOKEN_BUDGET)
    }

    /// Configured compaction summarizer, defaulting to the provider-backed one
    /// (ADR-0041). An unknown value falls back to the default rather than
    /// erroring, matching how other tuning knobs degrade.
    pub(crate) fn compaction_summarizer(&self) -> crate::wayland::SummarizerKind {
        match self.compaction_summarizer.as_deref() {
            Some("excerpts") => crate::wayland::SummarizerKind::Excerpts,
            Some("provider") | None => crate::wayland::SummarizerKind::Provider,
            Some(other) => {
                tracing::warn!(
                    value = other,
                    "unknown compactionSummarizer; using 'provider'"
                );
                crate::wayland::SummarizerKind::Provider
            }
        }
    }

    /// Whether opt-in microcompaction is enabled (ADR-0048, #378). Default off
    /// (absent/false), so a session folds spent tool results only when the user
    /// (or a project file) turns it on. The harness reads this once at startup;
    /// a `/settings` toggle takes effect at the next turn boundary.
    pub(crate) fn microcompaction(&self) -> bool {
        self.microcompaction.unwrap_or(false)
    }
}

/// Persist `provider`/`model` as the default model in the global settings file,
/// preserving every other key. Written to the user-global file (never project
/// config) so it is consistent with the global-only provider security rule.
pub(crate) fn save_default_model(provider: &str, model: &str) -> Result<()> {
    update_global(&[
        ("defaultProvider", Value::String(provider.to_string())),
        ("defaultModel", Value::String(model.to_string())),
    ])
}

/// The effective default model as a canonical `provider/model` id. The `/model`
/// picker uses it to label the "Default" row, which can differ from the active
/// session model after a session-only switch. Resolved through
/// [`ModelSelection::resolve`] over the global settings so it applies the same
/// provider/model precedence and built-in fallbacks as startup (and canonicalizes
/// the provider, so a hand-edited `defaultProvider` casing still matches the
/// catalog's lowercase qualified ids). Global-only, matching where
/// `save_default_model` writes; `None` only if the global path is unreadable or
/// settings are invalid.
pub(crate) fn default_model_qualified() -> Option<String> {
    let path = global_path()?;
    let settings = read_optional(&path).ok().flatten().unwrap_or_default();
    let resolved = crate::mimir::selection::ModelSelection::resolve(&settings).ok()?;
    Some(format!("{}/{}", resolved.provider.as_str(), resolved.model))
}

/// Persist the default reasoning/thinking level in the global settings file.
pub(crate) fn save_default_reasoning(level: &str) -> Result<()> {
    update_global(&[("defaultReasoning", Value::String(level.to_string()))])
}

/// Persist (or clear) the scoped-model cycle set in the global settings file.
/// `None` (or an empty list) removes `enabledModels` so the session cycles all
/// authenticated models again, matching pi-mono's Ctrl+S "all enabled" path.
pub(crate) fn save_enabled_models(ids: Option<&[String]>) -> Result<()> {
    let value = match ids {
        Some(ids) if !ids.is_empty() => {
            Value::Array(ids.iter().cloned().map(Value::String).collect())
        }
        // Null is the documented "remove this key" sentinel for `update_global`.
        _ => Value::Null,
    };
    update_global(&[("enabledModels", value)])
}

/// Persist the startup approval posture (`strict|auto|never`) in the global
/// settings file. GLOBAL-ONLY (like [`save_prompt_cache_retention`]): a cloned
/// project must never redirect the initial posture, so this always writes the
/// user-global file.
pub(crate) fn save_default_approval(mode: &str) -> Result<()> {
    update_global(&[("defaultApproval", Value::String(mode.to_string()))])
}

/// Persist the prompt-cache retention preset (`none|short|long`) in the global
/// settings file. GLOBAL-ONLY, matching where the field is trusted on load.
pub(crate) fn save_prompt_cache_retention(preset: &str) -> Result<()> {
    update_global(&[("promptCacheRetention", Value::String(preset.to_string()))])
}

/// Persist the context token budget in the global settings file, clamped to a
/// sane positive range so a hand-typed value cannot store a degenerate budget.
pub(crate) fn save_context_token_budget(budget: u64) -> Result<()> {
    let budget = budget.clamp(MIN_CONTEXT_TOKEN_BUDGET, MAX_CONTEXT_TOKEN_BUDGET);
    update_global(&[("contextTokenBudget", Value::from(budget))])
}

/// Persist the opt-in microcompaction toggle in the global settings file
/// (ADR-0048, #378). A boolean, so no clamping is needed; the `/settings` toggle
/// and config parsing both validate at the boundary.
pub(crate) fn save_microcompaction(enabled: bool) -> Result<()> {
    update_global(&[("microcompaction", Value::Bool(enabled))])
}

/// Persist (or clear) the tool round-trip soft cap in the global settings file.
/// `None` removes `maxToolRoundtrips` (unbounded loop); a value is clamped to a
/// sane positive range.
pub(crate) fn save_max_tool_roundtrips(cap: Option<usize>) -> Result<()> {
    let value = match cap {
        Some(cap) => {
            let cap = (cap as u64).clamp(MIN_TOOL_ROUNDTRIPS, MAX_TOOL_ROUNDTRIPS);
            Value::from(cap)
        }
        None => Value::Null,
    };
    update_global(&[("maxToolRoundtrips", value)])
}

/// Persist (or clear) the worktree-root preference in the global settings file.
/// An empty/`None` value removes `worktreeRoot` (fall back to `../wt`).
pub(crate) fn save_worktree_root(root: Option<&str>) -> Result<()> {
    update_global(&[("worktreeRoot", string_or_null(root))])
}

/// Persist the alt-screen policy (`auto|always|never`) under the `tui` block.
pub(crate) fn save_alt_screen(policy: &str) -> Result<()> {
    update_global_block("tui", &[("altScreen", Value::String(policy.to_string()))])
}

/// Persist the pager scroll speed under the `tui` block, clamped to `[1, 100]`.
pub(crate) fn save_scroll_speed(speed: u16) -> Result<()> {
    update_global_block("tui", &[("scrollSpeed", Value::from(speed.clamp(1, 100)))])
}

/// Persist the reduced-motion display preference under the `tui` block.
pub(crate) fn save_reduced_motion(enabled: bool) -> Result<()> {
    update_global_block("tui", &[("reducedMotion", Value::Bool(enabled))])
}

/// Persist the selected color theme id under the `tui` block (ADR-0042).
pub(crate) fn save_theme(theme: &str) -> Result<()> {
    update_global_block("tui", &[("theme", Value::String(theme.to_string()))])
}

/// Persist (or clear) the verification command under the `verify` block. An
/// empty/`None` command removes the key (feature reports skipped-unconfigured).
pub(crate) fn save_verify_command(command: Option<&str>) -> Result<()> {
    update_global_block("verify", &[("command", string_or_null(command))])
}

/// Persist the verification attempt cap under the `verify` block, clamped to
/// `[1, MAX_VERIFY_MAX_ATTEMPTS]` so a hand-typed value cannot request an
/// unbounded chain of effectful runs.
pub(crate) fn save_verify_max_attempts(attempts: u32) -> Result<()> {
    let attempts = attempts.clamp(1, MAX_VERIFY_MAX_ATTEMPTS);
    update_global_block("verify", &[("maxAttempts", Value::from(attempts))])
}

/// Sane lower/upper bounds for a persisted context token budget.
const MIN_CONTEXT_TOKEN_BUDGET: u64 = 1_000;
const MAX_CONTEXT_TOKEN_BUDGET: u64 = 100_000_000;
/// Sane lower/upper bounds for a persisted tool round-trip cap.
const MIN_TOOL_ROUNDTRIPS: u64 = 1;
const MAX_TOOL_ROUNDTRIPS: u64 = 1_000;

/// A trimmed non-empty string as a JSON string, or `Value::Null` (the
/// `update_global` "remove this key" sentinel) for an absent/blank value.
fn string_or_null(value: Option<&str>) -> Value {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => Value::String(v.to_string()),
        None => Value::Null,
    }
}

/// Read the global settings file as a raw JSON object, apply `updates` (a
/// `Value::Null` removes the key), and write it back atomically. Reading the raw
/// map -- rather than reserializing the typed [`Settings`] -- preserves any keys
/// this binary does not know about, so an older Iris never drops a newer config
/// field. A missing file starts from an empty object.
fn update_global(updates: &[(&str, Value)]) -> Result<()> {
    let path = global_path()
        .context("cannot resolve the global settings path (set HOME or IRIS_CONFIG_PATH)")?;
    let mut object = read_object(&path)?;
    for (key, value) in updates {
        if value.is_null() {
            object.remove(*key);
        } else {
            object.insert((*key).to_string(), value.clone());
        }
    }
    write_object_atomically(&path, &object)
}

/// Apply `updates` to a nested object `block` (e.g. `tui`, `verify`) in the
/// global settings file, preserving every other key inside and outside the
/// block. A `Value::Null` removes a key within the block; keys this binary does
/// not know about survive. The block is created on first write and left in place
/// (an empty block is harmless) so unrelated sibling keys are never dropped.
fn update_global_block(block: &str, updates: &[(&str, Value)]) -> Result<()> {
    let path = global_path()
        .context("cannot resolve the global settings path (set HOME or IRIS_CONFIG_PATH)")?;
    let mut object = read_object(&path)?;
    let nested = match object.remove(block) {
        Some(Value::Object(existing)) => existing,
        // A non-object (or absent) block is replaced with a fresh object rather
        // than silently merged into a scalar.
        _ => Map::new(),
    };
    let mut nested = nested;
    for (key, value) in updates {
        if value.is_null() {
            nested.remove(*key);
        } else {
            nested.insert((*key).to_string(), value.clone());
        }
    }
    object.insert(block.to_string(), Value::Object(nested));
    write_object_atomically(&path, &object)
}

/// Read a settings file as a JSON object, returning an empty object when the
/// file is absent. A present-but-non-object file is an error rather than a
/// silent overwrite, so a hand-edited config is never clobbered blindly.
fn read_object(path: &Path) -> Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str(&contents)
            .with_context(|| format!("invalid settings file {}", path.display()))?
        {
            Value::Object(object) => Ok(object),
            _ => bail!("settings file {} is not a JSON object", path.display()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Write the settings object to `path` via temp-file + rename so a crash never
/// leaves a half-written config.
fn write_object_atomically(path: &Path, object: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(object)?;
    let tmp = path.with_extension(format!(
        "tmp-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut file = std::fs::File::create(&tmp)
        .with_context(|| format!("failed to create {}", tmp.display()))?;
    // fsync before the rename: the rename is atomic, but without flushing the
    // file's data first a crash right after rename can expose a zero-length or
    // partially written settings.json.
    file.write_all(raw.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))
}

/// Parse a settings file, returning `None` when it does not exist.
fn read_optional(path: &Path) -> Result<Option<Settings>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let settings = serde_json::from_str(&contents)
        .with_context(|| format!("invalid settings file {}", path.display()))?;
    Ok(Some(settings))
}

/// Global settings path: `IRIS_CONFIG_PATH` override, else `~/.iris/settings.json`.
/// Mirrors the `IRIS_AUTH_PATH` / `~/.iris/auth.json` convention. Returns `None`
/// when neither `IRIS_CONFIG_PATH` nor `HOME` is set, so a missing `HOME` skips
/// the global layer rather than resolving to a relative `.iris/settings.json`
/// that would double-read the project file.
fn global_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("IRIS_CONFIG_PATH") {
        return Some(PathBuf::from(path));
    }
    let home = env::var("HOME").ok().filter(|home| !home.is_empty())?;
    Some(Path::new(&home).join(".iris/settings.json"))
}

fn project_path(cwd: &Path) -> PathBuf {
    cwd.join(".iris/settings.json")
}

/// Where `/debug` writes its snapshot: `~/.iris/iris-debug.log` (mirroring
/// pi-mono's `~/.pi/agent/pi-debug.log`). `None` when `HOME` is unset, so the
/// command reports the problem instead of writing a relative path.
pub(crate) fn debug_log_path() -> Option<PathBuf> {
    let home = env::var("HOME").ok().filter(|home| !home.is_empty())?;
    Some(Path::new(&home).join(".iris/iris-debug.log"))
}

/// Truthy reading of an `IRIS_*` opt-in environment variable, using the same
/// convention as `IRIS_SECURITY_OPT_IN` (`1`/`true`/`yes`/`on`). Lets the
/// accessibility switches (`IRIS_PLAIN`, `IRIS_REDUCED_MOTION`) share one parser
/// so they behave identically.
pub(crate) fn iris_flag_enabled(name: &str) -> bool {
    iris_flag_value(env::var(name).ok().as_deref())
}

fn iris_flag_value(value: Option<&str>) -> bool {
    matches!(value, Some("1" | "true" | "yes" | "on"))
}

/// Whether the working-indicator animation should be frozen: the
/// `IRIS_REDUCED_MOTION` env flag wins, else the persisted `tui.reducedMotion`
/// preference. `setting` is the loaded `tui.reducedMotion` (absent -> `None`).
pub(crate) fn reduced_motion_enabled(setting: Option<bool>) -> bool {
    reduced_motion_value(iris_flag_enabled("IRIS_REDUCED_MOTION"), setting)
}

fn reduced_motion_value(env_on: bool, setting: Option<bool>) -> bool {
    env_on || setting == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn worktree_root_default_is_normalized_beside_the_main_root() {
        let settings = Settings::default();
        // Default `../wt` resolves lexically, without a `..` component.
        assert_eq!(
            settings.worktree_root(Path::new("/repos/main")),
            PathBuf::from("/repos/wt")
        );
        // An absolute override is used as-is (still normalized).
        let abs = Settings {
            worktree_root: Some("/srv/trees/./x".to_string()),
            ..Settings::default()
        };
        assert_eq!(
            abs.worktree_root(Path::new("/repos/main")),
            PathBuf::from("/srv/trees/x")
        );
    }

    #[test]
    fn iris_flag_value_matches_the_opt_in_convention() {
        for on in ["1", "true", "yes", "on"] {
            assert!(iris_flag_value(Some(on)), "{on:?} should enable");
        }
        let off: [Option<&str>; 6] = [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("off"),
        ];
        for value in off {
            assert!(!iris_flag_value(value), "{value:?} should not enable");
        }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn temp_dir() -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("iris-config-test-{nanos}-{seq}"));
        fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    #[test]
    fn tui_alt_screen_parses_and_project_value_wins() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "tui": { "altScreen": "never" } }"#).unwrap();
        fs::write(&project, r#"{ "tui": { "altScreen": "auto" } }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(
            settings
                .tui_settings()
                .and_then(|t| t.alt_screen.as_deref()),
            Some("auto")
        );

        // Global-only config still surfaces, and an absent block yields None.
        let only_global = Settings::load_from(Some(&global), &dir.path.join("nope.json")).unwrap();
        assert_eq!(
            only_global
                .tui_settings()
                .and_then(|t| t.alt_screen.as_deref()),
            Some("never")
        );
        assert!(Settings::default().tui_settings().is_none());
    }

    #[test]
    fn missing_files_yield_default_settings() {
        let dir = temp_dir();
        let settings = Settings::load_from(
            Some(&dir.path.join("nope.json")),
            &dir.path.join("also-nope.json"),
        )
        .unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn microcompaction_defaults_off_and_a_project_may_tune_it() {
        // Default off: an unset key means no folds are written (ADR-0048).
        assert!(!Settings::default().microcompaction());

        // Project-tunable cost/quality knob: a project file may turn it on.
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, "{}").unwrap();
        fs::write(&project, r#"{ "microcompaction": true }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert!(settings.microcompaction());
    }

    #[test]
    fn absent_global_path_loads_only_project() {
        let dir = temp_dir();
        let project = dir.path.join("project.json");
        fs::write(&project, r#"{ "defaultModel": "project-model" }"#).unwrap();
        // None global path models a missing HOME: the project file still loads.
        let settings = Settings::load_from(None, &project).unwrap();
        assert_eq!(settings.default_model.as_deref(), Some("project-model"));
    }

    #[test]
    fn project_overrides_only_model() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{ "defaultProvider": "openai-codex", "defaultModel": "global-model", "baseUrl": "https://global.example" }"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"{ "defaultProvider": "antigravity", "defaultModel": "project-model", "baseUrl": "https://evil.example" }"#,
        )
        .unwrap();

        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(settings.default_provider.as_deref(), Some("openai-codex"));
        assert_eq!(settings.default_model.as_deref(), Some("project-model"));
        assert_eq!(settings.base_url.as_deref(), Some("https://global.example"));
    }

    #[test]
    fn project_may_override_reasoning_but_not_provider_or_base_url() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{ "defaultProvider": "openai-codex", "baseUrl": "https://global.example", "defaultReasoning": "low" }"#,
        )
        .unwrap();
        // A malicious project tries to redirect provider/base-url AND tune
        // reasoning. Only reasoning (and model, tested above) is trusted there.
        fs::write(
            &project,
            r#"{ "defaultProvider": "antigravity", "baseUrl": "https://evil.example", "defaultReasoning": "high" }"#,
        )
        .unwrap();

        let settings = Settings::load_from(Some(&global), &project).unwrap();
        // Security invariant: provider/base-url stay global-only.
        assert_eq!(settings.default_provider.as_deref(), Some("openai-codex"));
        assert_eq!(settings.base_url.as_deref(), Some("https://global.example"));
        // Reasoning is not a redirect, so the project override wins.
        assert_eq!(settings.default_reasoning.as_deref(), Some("high"));
    }

    #[test]
    fn context_token_budget_defaults_when_unset_and_parses_when_present() {
        let dir = temp_dir();
        // Unset -> built-in default, no error.
        let defaulted = Settings::load_from(
            Some(&dir.path.join("none.json")),
            &dir.path.join("none.json"),
        )
        .unwrap();
        assert_eq!(defaulted.context_token_budget, None);
        assert_eq!(
            defaulted.context_token_budget(),
            DEFAULT_CONTEXT_TOKEN_BUDGET
        );

        // Present -> parsed and surfaced; a project may tune it.
        let project = dir.path.join("project.json");
        fs::write(&project, r#"{ "contextTokenBudget": 64000 }"#).unwrap();
        let configured = Settings::load_from(None, &project).unwrap();
        assert_eq!(configured.context_token_budget, Some(64_000));
        assert_eq!(configured.context_token_budget(), 64_000);
    }

    #[test]
    fn max_tool_roundtrips_defaults_to_unbounded_and_a_project_may_set_it() {
        let dir = temp_dir();
        // Unset -> None (unbounded loop).
        let defaulted = Settings::load_from(
            Some(&dir.path.join("none.json")),
            &dir.path.join("none.json"),
        )
        .unwrap();
        assert_eq!(defaulted.max_tool_roundtrips(), None);

        // A project may set the soft cap (it can only narrow a run).
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "maxToolRoundtrips": 100 }"#).unwrap();
        fs::write(&project, r#"{ "maxToolRoundtrips": 20 }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(settings.max_tool_roundtrips(), Some(20));
    }

    #[test]
    fn retry_settings_parse_and_default_and_are_global_only() {
        let dir = temp_dir();
        // Unset -> an all-default (empty) raw set.
        let defaulted = Settings::load_from(
            Some(&dir.path.join("none.json")),
            &dir.path.join("none.json"),
        )
        .unwrap();
        assert_eq!(defaulted.retry_settings(), RetrySettings::default());

        // Present -> parsed; global-only (a cloned project cannot crank it up).
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{ "retry": { "maxRetries": 5, "baseDelayMs": 1000, "maxDelayMs": 30000 } }"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"{ "retry": { "maxRetries": 99, "baseDelayMs": 1 } }"#,
        )
        .unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        let retry = settings.retry_settings();
        assert_eq!(retry.max_retries, Some(5));
        assert_eq!(retry.base_delay_ms, Some(1000));
        assert_eq!(retry.max_delay_ms, Some(30000));
    }

    #[test]
    fn verification_absent_block_is_feature_off() {
        let dir = temp_dir();
        let settings = Settings::load_from(
            Some(&dir.path.join("none.json")),
            &dir.path.join("none.json"),
        )
        .unwrap();
        assert_eq!(settings.verification(), None);
    }

    #[test]
    fn verification_present_block_engages_and_clamps() {
        let dir = temp_dir();
        // A project may set the verify command (it grants no new capability).
        let project = dir.path.join("project.json");
        fs::write(
            &project,
            r#"{ "verify": { "command": "  cargo test  ", "maxAttempts": 99 } }"#,
        )
        .unwrap();
        let configured = Settings::load_from(None, &project).unwrap().verification();
        assert_eq!(
            configured,
            Some(VerificationConfig {
                command: Some("cargo test".to_string()),
                max_attempts: MAX_VERIFY_MAX_ATTEMPTS,
            })
        );

        // Present block, empty command -> engaged but no command (skipped path);
        // absent maxAttempts -> the built-in default.
        let empty = dir.path.join("empty.json");
        fs::write(&empty, r#"{ "verify": { "command": "   " } }"#).unwrap();
        assert_eq!(
            Settings::load_from(None, &empty).unwrap().verification(),
            Some(VerificationConfig {
                command: None,
                max_attempts: DEFAULT_VERIFY_MAX_ATTEMPTS,
            })
        );
    }

    #[test]
    fn verification_project_overrides_global() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "verify": { "command": "global-check" } }"#).unwrap();
        fs::write(&project, r#"{ "verify": { "command": "project-check" } }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(
            settings.verification().unwrap().command.as_deref(),
            Some("project-check")
        );
    }

    #[test]
    fn prompt_cache_retention_is_global_only() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "promptCacheRetention": "none" }"#).unwrap();
        fs::write(&project, r#"{ "promptCacheRetention": "long" }"#).unwrap();

        let settings = Settings::load_from(Some(&global), &project).unwrap();

        assert_eq!(settings.prompt_cache_retention.as_deref(), Some("none"));
    }

    #[test]
    fn anthropic_context_management_is_global_only() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{ "anthropicContextManagement": { "clearThinking": {} } }"#,
        )
        .unwrap();
        // A cloned project must not enable server-side context edits.
        fs::write(
            &project,
            r#"{ "anthropicContextManagement": { "clearToolUses": { "keepToolUses": 1 } } }"#,
        )
        .unwrap();

        let settings = Settings::load_from(Some(&global), &project).unwrap();

        assert_eq!(
            settings.anthropic_context_management,
            Some(serde_json::json!({ "clearThinking": {} }))
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        fs::write(
            &global,
            r#"{ "defaultModel": "m", "futureKnob": 42, "nested": { "x": 1 } }"#,
        )
        .unwrap();
        let settings = Settings::load_from(Some(&global), &dir.path.join("nope.json")).unwrap();
        assert_eq!(settings.default_model.as_deref(), Some("m"));
    }

    #[test]
    fn update_global_preserves_unknown_keys_and_round_trips() {
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        std::fs::write(&path, r#"{ "futureKnob": 42, "defaultModel": "old" }"#).unwrap();

        // Set known keys; the unknown key must survive.
        let mut object = read_object(&path).unwrap();
        object.insert(
            "defaultProvider".to_string(),
            Value::String("anthropic".to_string()),
        );
        object.insert(
            "defaultModel".to_string(),
            Value::String("claude-sonnet-4-6".to_string()),
        );
        write_object_atomically(&path, &object).unwrap();

        let reread = read_object(&path).unwrap();
        assert_eq!(reread.get("futureKnob"), Some(&Value::from(42)));
        assert_eq!(
            reread.get("defaultProvider"),
            Some(&Value::String("anthropic".to_string()))
        );
        assert_eq!(
            reread.get("defaultModel"),
            Some(&Value::String("claude-sonnet-4-6".to_string()))
        );
        // And the typed loader reads it back.
        let settings = Settings::load_from(Some(&path), &dir.path.join("none.json")).unwrap();
        assert_eq!(settings.default_provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn read_object_missing_file_is_empty_and_non_object_errors() {
        let dir = temp_dir();
        assert!(
            read_object(&dir.path.join("absent.json"))
                .unwrap()
                .is_empty()
        );
        let path = dir.path.join("array.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        let err = read_object(&path).unwrap_err().to_string();
        assert!(err.contains("not a JSON object"), "{err}");
    }

    #[test]
    fn config_cannot_activate_dangerously_skip_permissions() {
        // ADR-0049 activation isolation: the skip-permissions mode is CLI-only.
        // Settings has no field for it, so a malicious global OR project config
        // carrying `dangerouslySkipPermissions: true` is inert -- serde ignores
        // the unknown key and the loaded Settings is byte-equal to the default.
        // There is intentionally no accessor or field a config could populate.
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        std::fs::write(&global, r#"{ "dangerouslySkipPermissions": true }"#).unwrap();
        std::fs::write(&project, r#"{ "dangerouslySkipPermissions": true }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(
            settings,
            Settings::default(),
            "an unknown skip-permissions key must not populate any setting"
        );
    }

    #[test]
    fn enabled_models_is_global_only() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        std::fs::write(&global, r#"{ "enabledModels": ["openai-codex/gpt-5.5"] }"#).unwrap();
        // A project file cannot inject a cycle scope.
        std::fs::write(&project, r#"{ "enabledModels": ["anthropic/evil"] }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(
            settings.enabled_models.as_deref(),
            Some(["openai-codex/gpt-5.5".to_string()].as_slice())
        );
    }

    #[test]
    fn reduced_motion_value_env_wins_then_setting() {
        assert!(reduced_motion_value(true, None), "env flag wins");
        assert!(
            reduced_motion_value(true, Some(false)),
            "env flag still wins"
        );
        assert!(reduced_motion_value(false, Some(true)), "setting honored");
        assert!(!reduced_motion_value(false, None), "neither set");
        assert!(!reduced_motion_value(false, Some(false)), "explicit off");
    }

    #[test]
    fn default_approval_is_global_only() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "defaultApproval": "strict" }"#).unwrap();
        // A cloned project must never lower the posture to `never`.
        fs::write(&project, r#"{ "defaultApproval": "never" }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(settings.default_approval.as_deref(), Some("strict"));
        // Absent global -> None (today's default applies at startup).
        let absent = Settings::load_from(Some(&dir.path.join("none.json")), &project).unwrap();
        assert_eq!(absent.default_approval, None);
    }

    #[test]
    fn tui_reduced_motion_loads_and_project_may_set() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "tui": { "reducedMotion": false } }"#).unwrap();
        fs::write(&project, r#"{ "tui": { "reducedMotion": true } }"#).unwrap();
        // Display preferences: the project block wins (project.tui.or(global)).
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        let tui = settings.tui_settings().unwrap();
        assert_eq!(tui.reduced_motion, Some(true));
        // Global-only surfaces when there is no project block.
        let only_global = Settings::load_from(Some(&global), &dir.path.join("nope.json")).unwrap();
        let tui = only_global.tui_settings().unwrap();
        assert_eq!(tui.reduced_motion, Some(false));
    }

    #[test]
    fn tui_theme_loads_from_global_block() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        fs::write(&global, r#"{ "tui": { "theme": "gruvbox" } }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &dir.path.join("nope.json")).unwrap();
        let tui = settings.tui_settings().unwrap();
        assert_eq!(tui.theme.as_deref(), Some("gruvbox"));
    }

    /// Point `IRIS_CONFIG_PATH` at a temp file for a save round-trip, restoring
    /// any previous value afterward. The save helpers write the user-global file,
    /// which this override selects.
    /// Serializes the save-round-trip tests: `IRIS_CONFIG_PATH` is process-
    /// global, so tests that point it at a temp file must not run concurrently.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ConfigPathGuard {
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ConfigPathGuard {
        fn set(path: &Path) -> Self {
            let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = env::var("IRIS_CONFIG_PATH").ok();
            // SAFETY: the ENV_LOCK guard serializes all IRIS_CONFIG_PATH writers;
            // restored on drop.
            unsafe { env::set_var("IRIS_CONFIG_PATH", path) };
            ConfigPathGuard { prev, _lock }
        }
    }

    impl Drop for ConfigPathGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(prev) => unsafe { env::set_var("IRIS_CONFIG_PATH", prev) },
                None => unsafe { env::remove_var("IRIS_CONFIG_PATH") },
            }
        }
    }

    #[test]
    fn save_helpers_round_trip_clamp_and_preserve_unknown_keys() {
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        fs::write(
            &path,
            r#"{ "futureKnob": 7, "tui": { "altScreen": "auto", "futureTui": 1 } }"#,
        )
        .unwrap();
        let _guard = ConfigPathGuard::set(&path);

        // Top-level scalar saves.
        save_default_approval("auto").unwrap();
        save_prompt_cache_retention("long").unwrap();
        // Clamp: below/above the sane range is pulled into it.
        save_context_token_budget(1).unwrap();
        save_max_tool_roundtrips(Some(99_999)).unwrap();
        save_worktree_root(Some("  ../trees  ")).unwrap();
        // Nested block saves preserve sibling + unknown nested keys.
        save_alt_screen("always").unwrap();
        save_scroll_speed(500).unwrap();
        save_reduced_motion(true).unwrap();
        save_verify_command("  cargo test  ".into()).unwrap();
        save_verify_max_attempts(99).unwrap();

        let object = read_object(&path).unwrap();
        assert_eq!(object.get("futureKnob"), Some(&Value::from(7)));
        assert_eq!(
            object.get("defaultApproval"),
            Some(&Value::String("auto".into()))
        );
        assert_eq!(object.get("contextTokenBudget"), Some(&Value::from(1_000)));
        assert_eq!(object.get("maxToolRoundtrips"), Some(&Value::from(1_000)));
        assert_eq!(
            object.get("worktreeRoot"),
            Some(&Value::String("../trees".into()))
        );
        let tui = object.get("tui").and_then(Value::as_object).unwrap();
        assert_eq!(tui.get("altScreen"), Some(&Value::String("always".into())));
        assert_eq!(tui.get("scrollSpeed"), Some(&Value::from(100)));
        assert_eq!(tui.get("reducedMotion"), Some(&Value::Bool(true)));
        assert_eq!(
            tui.get("futureTui"),
            Some(&Value::from(1)),
            "nested unknown kept"
        );
        let verify = object.get("verify").and_then(Value::as_object).unwrap();
        assert_eq!(
            verify.get("command"),
            Some(&Value::String("cargo test".into()))
        );
        assert_eq!(verify.get("maxAttempts"), Some(&Value::from(10)));
    }

    #[test]
    fn save_empty_values_clear_their_keys() {
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        fs::write(
            &path,
            r#"{ "maxToolRoundtrips": 20, "worktreeRoot": "../wt", "verify": { "command": "x", "maxAttempts": 2 } }"#,
        )
        .unwrap();
        let _guard = ConfigPathGuard::set(&path);

        save_max_tool_roundtrips(None).unwrap();
        save_worktree_root(None).unwrap();
        save_verify_command(None).unwrap();

        let object = read_object(&path).unwrap();
        assert!(
            !object.contains_key("maxToolRoundtrips"),
            "cleared to unbounded"
        );
        assert!(!object.contains_key("worktreeRoot"), "cleared to default");
        let verify = object.get("verify").and_then(Value::as_object).unwrap();
        assert!(!verify.contains_key("command"), "command cleared");
        assert_eq!(
            verify.get("maxAttempts"),
            Some(&Value::from(2)),
            "sibling kept"
        );
    }

    #[test]
    fn malformed_file_is_an_error_naming_the_path() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        fs::write(&global, "{ not json").unwrap();
        let err = Settings::load_from(Some(&global), &dir.path.join("nope.json"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid settings file"), "{err}");
        assert!(err.contains("global.json"), "{err}");
    }
}
