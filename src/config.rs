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
//! Project settings may tune local, non-credential behavior. Provider/base-url
//! are intentionally user-global so a cloned repository cannot redirect OAuth
//! bearer tokens to a malicious endpoint. Explicit runtime input still wins over
//! the file where a provider supports env overrides. Unknown keys are ignored so
//! older binaries tolerate newer config. A malformed file is a hard error -- a
//! silently-ignored config is a footgun.
//!
//! Live project grants are not configured here: project permission grants (`/trust`)
//! are HOME-owned state, not repo settings. The one exception is
//! `defaultApproval`, the global startup permission mode. It may be
//! `strict|auto|never|dangerously-skip-permissions`, but remains GLOBAL-ONLY (a
//! cloned project must never be able to weaken it -- see `merged_with`).
//! Cross-session approval-grant persistence is still tracked separately
//! (roadmap #14).

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

mod tool_result_compaction;

pub(crate) use tool_result_compaction::{
    CompactionAggressiveness, CompactionCacheTiming, ToolClearingBackend, ToolClearingMode,
    ToolResultCompactionPolicy, ToolResultCompactionSettings,
};

/// Settings loaded from the JSON config files. Every field is optional; an
/// absent field falls back to the next layer (safe project fields -> global ->
/// built-in default, with env applied above where the provider supports it).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
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
    /// Legacy absolute auto-compaction window override. When set, it clamps the
    /// model-derived effective window; when the model window is unknown it is
    /// the window. Kept on `contextTokenBudget` for compatibility.
    pub(crate) context_token_budget: Option<u64>,
    /// Microcompaction watermark. When microcompaction is enabled, detected fold
    /// plans flush once provider-visible context reaches this independent token
    /// threshold. Absent -> [`Settings::microcompaction_watermark`] default.
    pub(crate) microcompaction_watermark: Option<u64>,
    /// Default reasoning/thinking effort (`off|minimal|low|medium|high|xhigh|max`),
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
    /// How compaction produces its summary text: `provider` (default) uses the
    /// active model directly; `subagent` asks a read-only background worker for
    /// a structured handoff summary; `excerpts` keeps the deterministic
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
    /// Structured, opt-in tool-result compaction policy. Local semantic dedupe
    /// and clearing knobs are project-tunable; provider-native backend controls
    /// remain global-only in [`Settings::merged_with`]. When absent, the legacy
    /// `microcompaction` + `microcompactionWatermark` pair resolves to the
    /// conservative policy unchanged.
    pub(crate) tool_result_compaction: Option<ToolResultCompactionSettings>,
    /// Master mutation-safety switch. Global-only: a cloned project cannot
    /// disable host-owned dirty-tree protection. Absent defaults on.
    pub(crate) mutation_safety: Option<bool>,
    /// Opt-in durable task workflow (ADR-0052, issue #444): checkpoint refs,
    /// recovery/adoption, task lifecycle entries, badges, task diffs, and
    /// rollback across restarts. Effective only while mutation safety is on.
    pub(crate) tasks: Option<bool>,
    /// Opt-in bash tool mode: the model-visible tool set shrinks to `bash` and
    /// `edit` (plus the session-plumbing `read_output`/`recall`), so the model
    /// drives file inspection, listing, search, and file creation through the
    /// shell. Absent/false -> off (full built-in surface). Not a
    /// security-sensitive redirect: it only removes tools, and `bash` stays
    /// approval-per-call (ADR-0010), so a project file may tune it like
    /// [`Settings::microcompaction`].
    pub(crate) bash_tool_mode: Option<bool>,
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
    /// OpenAI Codex transport mode: `auto` uses OAuth WebSocket by default with
    /// HTTP/SSE fallback; `sse` forces the legacy HTTP/SSE route. GLOBAL-ONLY:
    /// it changes secret-bearing provider transport behavior and fallback policy.
    pub(crate) codex_transport: Option<String>,
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
    /// Startup permission mode (`strict|auto|never|dangerously-skip-permissions`).
    /// Normal modes are parsed by [`crate::nexus::ApprovalMode::parse`]; the
    /// dangerous token is handled by the host as the explicit skip-permissions
    /// mode. An absent/invalid value leaves today's default (`strict`).
    /// GLOBAL-ONLY: a cloned project must never be able to lower the initial
    /// posture or enable dangerous skip, so (like `prompt_cache_retention`) it is
    /// taken from global config and never from an untrusted project file.
    pub(crate) default_approval: Option<String>,
    /// Where the git dropdown's `w` (new worktree) gesture creates worktrees,
    /// relative to the main worktree root when not absolute. Absent ->
    /// `../wt`. Project-tunable: it only picks a local directory for a
    /// user-confirmed `git worktree add` (the resolved path is always shown
    /// before create), granting no new capability.
    pub(crate) worktree_root: Option<String>,
    /// Automatic full-context compaction policy. Project-safe tuning fields
    /// merge over global settings; routing/native fields remain global-only.
    pub(crate) compaction: Option<CompactionSettings>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactionSettings {
    pub(crate) enabled: Option<bool>,
    pub(crate) thresholds: Option<CompactionThresholdSettings>,
    pub(crate) keep_recent_tokens: Option<u64>,
    pub(crate) worker: Option<CompactionWorkerSettings>,
    pub(crate) hard_wait_ms: Option<u64>,
    pub(crate) max_consecutive_failures: Option<u32>,
    pub(crate) reactive: Option<bool>,
    pub(crate) provider_native: Option<String>,
    pub(crate) instructions: Option<String>,
    pub(crate) model_tool: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct CompactionThresholdSettings {
    pub(crate) warn: Option<f64>,
    pub(crate) start: Option<f64>,
    pub(crate) hard: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactionWorkerSettings {
    pub(crate) input: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) max_tool_roundtrips: Option<usize>,
    pub(crate) timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CompactionTriggerConfig {
    pub(crate) enabled: bool,
    pub(crate) warn: f64,
    pub(crate) start: f64,
    pub(crate) hard: f64,
    pub(crate) keep_recent_tokens: u64,
    pub(crate) hard_wait_ms: u64,
    pub(crate) max_consecutive_failures: u32,
    pub(crate) reactive: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ProviderNativeMode {
    #[default]
    Off,
    Auto,
}

fn merge_compaction(
    global: Option<CompactionSettings>,
    project: Option<CompactionSettings>,
) -> Option<CompactionSettings> {
    if global.is_none() && project.is_none() {
        return None;
    }
    let global = global.unwrap_or_default();
    let project = project.unwrap_or_default();
    let global_thresholds = global.thresholds.unwrap_or_default();
    let project_thresholds = project.thresholds.unwrap_or_default();
    let thresholds = CompactionThresholdSettings {
        warn: project_thresholds.warn.or(global_thresholds.warn),
        start: project_thresholds.start.or(global_thresholds.start),
        hard: project_thresholds.hard.or(global_thresholds.hard),
    };
    let global_worker = global.worker.unwrap_or_default();
    let project_worker = project.worker.unwrap_or_default();
    Some(CompactionSettings {
        enabled: project.enabled.or(global.enabled),
        thresholds: Some(thresholds),
        keep_recent_tokens: project.keep_recent_tokens.or(global.keep_recent_tokens),
        worker: Some(CompactionWorkerSettings {
            input: project_worker.input.or(global_worker.input),
            // Worker model changes provider routing and remains global-only.
            model: global_worker.model,
            max_tool_roundtrips: project_worker
                .max_tool_roundtrips
                .or(global_worker.max_tool_roundtrips),
            timeout_ms: project_worker.timeout_ms.or(global_worker.timeout_ms),
        }),
        hard_wait_ms: project.hard_wait_ms.or(global.hard_wait_ms),
        max_consecutive_failures: project
            .max_consecutive_failures
            .or(global.max_consecutive_failures),
        reactive: project.reactive.or(global.reactive),
        // Provider-native rewrites alter server-side semantics and are global-only.
        provider_native: global.provider_native,
        instructions: project.instructions.or(global.instructions),
        model_tool: project.model_tool.or(global.model_tool),
    })
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

/// Legacy fallback when neither an explicit budget nor a model window exists.
const DEFAULT_CONTEXT_TOKEN_BUDGET: u64 = 128_000;
pub(crate) const DEFAULT_SUMMARY_RESERVE: u64 = 8_192;
pub(crate) const DEFAULT_COMPACTION_WARN: f64 = 0.60;
pub(crate) const DEFAULT_COMPACTION_START: f64 = 0.72;
pub(crate) const DEFAULT_COMPACTION_HARD: f64 = 0.90;
pub(crate) const DEFAULT_COMPACTION_KEEP_RECENT_TOKENS: u64 = 8_000;
pub(crate) const DEFAULT_COMPACTION_HARD_WAIT_MS: u64 = 120_000;
const MAX_COMPACTION_HARD_WAIT_MS: u64 = 300_000;
const DEFAULT_COMPACTION_MAX_FAILURES: u32 = 3;
/// Default independent microcompaction flush threshold. This matches the old
/// default `contextTokenBudget / 2` behavior without coupling future budget edits
/// to microcompaction.
const DEFAULT_MICROCOMPACTION_WATERMARK: u64 = 64_000;

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
        let tool_result_compaction = tool_result_compaction::merge(
            self.tool_result_compaction.clone(),
            project.tool_result_compaction.clone(),
        );
        let compaction = merge_compaction(self.compaction.clone(), project.compaction.clone());
        Settings {
            default_provider: self.default_provider,
            default_model: project.default_model.or(self.default_model),
            base_url: self.base_url,
            // A budget is not a security-sensitive redirect (unlike provider /
            // base-url), so a project may tune it; fall back to global, then the
            // built-in default via the accessor.
            context_token_budget: project.context_token_budget.or(self.context_token_budget),
            microcompaction_watermark: project
                .microcompaction_watermark
                .or(self.microcompaction_watermark),
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
            tool_result_compaction,
            // Mutation safety is host posture and therefore global-only. Repo
            // config can never turn the guard off.
            mutation_safety: self.mutation_safety,
            // Durable task workflow is opt-in product surface, not the safety
            // floor. Project config may enable it for a repo; absent defaults
            // off via the accessor.
            tasks: project.tasks.or(self.tasks),
            // Bash tool mode only removes tools from the model-visible surface
            // and bash stays approval-per-call, so a project may tune it;
            // project value wins, else global.
            bash_tool_mode: project.bash_tool_mode.or(self.bash_tool_mode),
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
            // Codex transport affects secret-bearing provider transport and
            // fallback behavior, so it is global-only like provider/base-url.
            codex_transport: self.codex_transport,
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
            // Startup permission mode gates whether tools auto-run without a
            // prompt and may enable dangerous skip, so it is GLOBAL-ONLY: a
            // cloned project must never lower the initial posture.
            default_approval: self.default_approval,
            // A local worktree location preference; project value wins.
            worktree_root: project.worktree_root.or(self.worktree_root),
            compaction,
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

    /// Explicit legacy window override, or the unknown-model fallback when
    /// unset. Production model-aware resolution preserves raw `None` separately.
    pub(crate) fn context_token_budget(&self) -> u64 {
        self.context_token_budget
            .unwrap_or(DEFAULT_CONTEXT_TOKEN_BUDGET)
    }

    /// Validated trigger-ladder tuning. The legacy absolute budget is checked
    /// here because `enabled=false`, not a zero budget, is the off switch.
    pub(crate) fn compaction_trigger(&self) -> Result<CompactionTriggerConfig> {
        if let Some(budget) = self.context_token_budget
            && budget < DEFAULT_SUMMARY_RESERVE
        {
            bail!(
                "contextTokenBudget must be at least {DEFAULT_SUMMARY_RESERVE}; use compaction.enabled=false to disable automatic compaction"
            );
        }
        let compaction = self.compaction.as_ref();
        let thresholds = compaction.and_then(|value| value.thresholds.as_ref());
        let warn = thresholds
            .and_then(|value| value.warn)
            .unwrap_or(DEFAULT_COMPACTION_WARN);
        let start = thresholds
            .and_then(|value| value.start)
            .unwrap_or(DEFAULT_COMPACTION_START);
        let hard = thresholds
            .and_then(|value| value.hard)
            .unwrap_or(DEFAULT_COMPACTION_HARD);
        if !(warn.is_finite()
            && start.is_finite()
            && hard.is_finite()
            && 0.0 < warn
            && warn < start
            && start < hard
            && hard < 1.0)
        {
            bail!("compaction thresholds must satisfy 0 < warn < start < hard < 1");
        }
        let keep_recent_tokens = compaction
            .and_then(|value| value.keep_recent_tokens)
            .unwrap_or(DEFAULT_COMPACTION_KEEP_RECENT_TOKENS);
        if keep_recent_tokens == 0 {
            bail!("compaction.keepRecentTokens must be greater than zero");
        }
        let max_consecutive_failures = compaction
            .and_then(|value| value.max_consecutive_failures)
            .unwrap_or(DEFAULT_COMPACTION_MAX_FAILURES);
        if max_consecutive_failures == 0 {
            bail!("compaction.maxConsecutiveFailures must be greater than zero");
        }
        let hard_wait_ms = compaction
            .and_then(|value| value.hard_wait_ms)
            .unwrap_or(DEFAULT_COMPACTION_HARD_WAIT_MS);
        if hard_wait_ms > MAX_COMPACTION_HARD_WAIT_MS {
            bail!(
                "compaction.hardWaitMs must be at most {MAX_COMPACTION_HARD_WAIT_MS} milliseconds"
            );
        }
        Ok(CompactionTriggerConfig {
            enabled: compaction.and_then(|value| value.enabled).unwrap_or(true),
            warn,
            start,
            hard,
            keep_recent_tokens,
            hard_wait_ms,
            max_consecutive_failures,
            reactive: compaction.and_then(|value| value.reactive).unwrap_or(true),
        })
    }

    /// Configured independent microcompaction watermark, or the built-in default
    /// when unset. The fold scheduler uses this as its Class C flush backstop.
    pub(crate) fn microcompaction_watermark(&self) -> u64 {
        self.microcompaction_watermark
            .unwrap_or(DEFAULT_MICROCOMPACTION_WATERMARK)
    }

    /// Configured compaction summarizer, defaulting to the active provider.
    /// Provider-native compaction takes precedence when supported; this setting
    /// is the portable fallback for unsupported selections.
    /// An unknown value falls back to the default rather than erroring, matching
    /// how other tuning knobs degrade.
    pub(crate) fn compaction_summarizer(&self) -> crate::wayland::SummarizerKind {
        match self.compaction_summarizer.as_deref() {
            Some("excerpts") => crate::wayland::SummarizerKind::Excerpts,
            Some("provider") | None => crate::wayland::SummarizerKind::Provider,
            Some("subagent") => crate::wayland::SummarizerKind::Subagent,
            Some(other) => {
                tracing::warn!(
                    value = other,
                    "unknown compactionSummarizer; using 'provider'"
                );
                crate::wayland::SummarizerKind::Provider
            }
        }
    }

    pub(crate) fn compaction_worker_config(
        &self,
    ) -> Result<crate::wayland::CompactionWorkerConfig> {
        let compaction = self.compaction.as_ref();
        let worker = compaction.and_then(|value| value.worker.as_ref());
        let input = match worker.and_then(|value| value.input.as_deref()) {
            Some("transcript") | None => crate::wayland::CompactionWorkerInput::Transcript,
            Some("investigator") => crate::wayland::CompactionWorkerInput::Investigator,
            Some(_) => bail!("compaction.worker.input must be transcript|investigator"),
        };
        let max_tool_roundtrips = worker
            .and_then(|value| value.max_tool_roundtrips)
            .unwrap_or(crate::wayland::SUMMARY_WORKER_MAX_TOOL_ROUNDTRIPS);
        if max_tool_roundtrips == 0 {
            bail!("compaction.worker.maxToolRoundtrips must be greater than zero");
        }
        let timeout_ms = worker.and_then(|value| value.timeout_ms).unwrap_or(120_000);
        if timeout_ms == 0 {
            bail!("compaction.worker.timeoutMs must be greater than zero");
        }
        let instructions = compaction
            .and_then(|value| value.instructions.as_deref())
            .unwrap_or_default()
            .trim()
            .chars()
            .take(crate::wayland::MAX_COMPACTION_INSTRUCTIONS_CHARS)
            .collect();
        Ok(crate::wayland::CompactionWorkerConfig {
            input,
            max_tool_roundtrips,
            timeout: std::time::Duration::from_millis(timeout_ms),
            instructions,
        })
    }

    pub(crate) fn compaction_worker_model(&self) -> Option<&str> {
        self.compaction
            .as_ref()?
            .worker
            .as_ref()?
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    pub(crate) fn compaction_provider_native(&self) -> Result<ProviderNativeMode> {
        match self
            .compaction
            .as_ref()
            .and_then(|value| value.provider_native.as_deref())
            .map(str::trim)
        {
            Some("off") => Ok(ProviderNativeMode::Off),
            None | Some("auto") => Ok(ProviderNativeMode::Auto),
            Some(_) => bail!("compaction.providerNative must be off|auto"),
        }
    }

    /// Whether the model sees the request-only compaction tool. Project-safe:
    /// it schedules the existing governor and grants no provider, filesystem,
    /// shell, or approval capability.
    pub(crate) fn compaction_model_tool(&self) -> bool {
        self.compaction
            .as_ref()
            .and_then(|value| value.model_tool)
            .unwrap_or(false)
    }

    /// Whether opt-in microcompaction is enabled (ADR-0048, #378). Default off
    /// (absent/false), so a session folds spent tool results only when the user
    /// (or a project file) turns it on. The harness reads this once at startup;
    /// a `/settings` toggle takes effect at the next turn boundary.
    pub(crate) fn microcompaction(&self) -> bool {
        self.microcompaction.unwrap_or(false)
    }

    /// Resolve the structured policy, or the legacy conservative alias when no
    /// structured block exists. This is the single validation boundary shared
    /// by Wayland and Mimir; malformed enum/count/tool-list values fail before
    /// a provider is built or a fold is planned.
    pub(crate) fn tool_result_compaction(&self) -> Result<ToolResultCompactionPolicy> {
        tool_result_compaction::resolve(
            self.tool_result_compaction.as_ref(),
            self.microcompaction(),
            self.microcompaction_watermark(),
        )
    }

    /// Whether mutation safety gates are enabled. Default on. This global-only
    /// master controls dirty-file approval, snapshots, attribution, restoration,
    /// and task settlement integration.
    pub(crate) fn mutation_safety(&self) -> bool {
        self.mutation_safety.unwrap_or(true)
    }

    /// Whether the durable task workflow is enabled. Default off and meaningful
    /// only while mutation safety is enabled.
    pub(crate) fn tasks(&self) -> bool {
        self.tasks.unwrap_or(false)
    }

    /// Whether bash tool mode is enabled: only `bash` and `edit` (plus the
    /// session-plumbing `read_output`/`recall`) are registered, so the model
    /// uses the shell for file operations. Default off (full tool surface).
    /// Read once at startup when the tool set is constructed; a `/settings`
    /// toggle takes effect at the next session start.
    pub(crate) fn bash_tool_mode(&self) -> bool {
        self.bash_tool_mode.unwrap_or(false)
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

/// Persist the startup permission mode
/// (`strict|auto|never|dangerously-skip-permissions`) in the global settings
/// file. GLOBAL-ONLY (like [`save_prompt_cache_retention`]): a cloned project
/// must never redirect the initial posture, so this always writes the user-global
/// file.
pub(crate) fn save_default_approval(mode: &str) -> Result<()> {
    update_global(&[("defaultApproval", Value::String(mode.to_string()))])
}

/// Persist the global mutation-safety master switch. This never writes project
/// config, so repository-controlled settings cannot weaken host protection.
pub(crate) fn save_mutation_safety(enabled: bool) -> Result<()> {
    update_global(&[("mutationSafety", Value::Bool(enabled))])
}

/// Persist the prompt-cache retention preset (`none|short|long`) in the global
/// settings file. GLOBAL-ONLY, matching where the field is trusted on load.
pub(crate) fn save_prompt_cache_retention(preset: &str) -> Result<()> {
    update_global(&[("promptCacheRetention", Value::String(preset.to_string()))])
}

/// Persist the compaction summarizer mode (`excerpts|provider|subagent`) in the
/// global settings file. Project files may still override it at load; this is the
/// user-facing `/settings` write path.
pub(crate) fn save_compaction_summarizer(mode: &str) -> Result<()> {
    let mode = match mode {
        "excerpts" | "provider" | "subagent" => mode,
        _ => "subagent",
    };
    update_global(&[("compactionSummarizer", Value::String(mode.to_string()))])
}

/// Persist the legacy context window override. Values below the summary reserve
/// are rejected; disabling automatic compaction is an explicit boolean policy.
pub(crate) fn save_context_token_budget(budget: u64) -> Result<()> {
    if budget < DEFAULT_SUMMARY_RESERVE {
        bail!(
            "contextTokenBudget must be at least {DEFAULT_SUMMARY_RESERVE}; use compaction.enabled=false to disable automatic compaction"
        );
    }
    let budget = budget.min(MAX_CONTEXT_TOKEN_BUDGET);
    update_global(&[("contextTokenBudget", Value::from(budget))])
}

/// Persist the microcompaction watermark in the global settings file, clamped to
/// the same sane positive token range as the auto-compaction threshold.
#[cfg(test)]
pub(crate) fn save_microcompaction_watermark(watermark: u64) -> Result<()> {
    let watermark = watermark.clamp(MIN_CONTEXT_TOKEN_BUDGET, MAX_CONTEXT_TOKEN_BUDGET);
    update_global(&[("microcompactionWatermark", Value::from(watermark))])
}

/// Persist the opt-in microcompaction toggle in the global settings file
/// (ADR-0048, #378). A boolean, so no clamping is needed; the `/settings` toggle
/// and config parsing both validate at the boundary.
///
/// Enabling is rejected when legacy Anthropic clearing overlaps the legacy
/// semantic candidate set (`read`, `ls`). Excluding both tools proves the
/// reducers disjoint and is accepted. Typed selection validation applies the
/// same rule to the merged structured policy.
#[cfg(test)]
pub(crate) fn save_microcompaction(enabled: bool) -> Result<()> {
    if enabled {
        let path = global_path()
            .context("cannot resolve the global settings path (set HOME or IRIS_CONFIG_PATH)")?;
        let object = read_object(&path)?;
        let clear_tool_uses = object
            .get("anthropicContextManagement")
            .and_then(|value| value.get("clearToolUses"));
        let excludes_local_c = clear_tool_uses
            .and_then(|value| value.get("excludeTools"))
            .and_then(Value::as_array)
            .is_some_and(|tools| {
                ["read", "ls"]
                    .iter()
                    .all(|required| tools.iter().any(|tool| tool.as_str() == Some(required)))
            });
        if clear_tool_uses.is_some() && !excludes_local_c {
            anyhow::bail!(
                "anthropicContextManagement.clearToolUses and microcompaction cannot be enabled \
                 together for overlapping tools; exclude both read and ls from clearToolUses, \
                 or disable one reducer (clearThinking remains compatible)."
            );
        }
    }
    update_global(&[("microcompaction", Value::Bool(enabled))])
}

pub(crate) fn save_tool_result_compaction_enabled(enabled: bool) -> Result<()> {
    update_global_nested(
        &["toolResultCompaction"],
        &[("enabled", Value::Bool(enabled))],
        SaveValidation::ToolResultCompaction,
    )
}

pub(crate) fn save_tool_result_compaction_aggressiveness(value: &str) -> Result<()> {
    let value = CompactionAggressiveness::parse(Some(value))?.as_str();
    update_global_nested(
        &["toolResultCompaction"],
        &[("aggressiveness", Value::String(value.to_string()))],
        SaveValidation::ToolResultCompaction,
    )
}

pub(crate) fn save_tool_result_compaction_cache_timing(value: &str) -> Result<()> {
    let value = CompactionCacheTiming::parse(Some(value))?.as_str();
    update_global_nested(
        &["toolResultCompaction"],
        &[("cacheTiming", Value::String(value.to_string()))],
        SaveValidation::ToolResultCompaction,
    )
}

pub(crate) fn save_tool_result_compaction_trigger_tokens(tokens: u64) -> Result<()> {
    let tokens = tokens.clamp(MIN_CONTEXT_TOKEN_BUDGET, MAX_CONTEXT_TOKEN_BUDGET);
    update_global_nested(
        &["toolResultCompaction"],
        &[("triggerTokens", Value::from(tokens))],
        SaveValidation::ToolResultCompaction,
    )
}

pub(crate) fn save_tool_result_compaction_retain_per_path(retain: u64) -> Result<()> {
    update_global_nested(
        &["toolResultCompaction", "semanticDedupe"],
        &[("retainPerPath", Value::from(retain.max(1)))],
        SaveValidation::ToolResultCompaction,
    )
}

pub(crate) fn save_tool_result_compaction_keep_recent_tool_uses(keep: u64) -> Result<()> {
    update_global_nested(
        &["toolResultCompaction", "toolClearing"],
        &[("keepRecentToolUses", Value::from(keep.max(1)))],
        SaveValidation::ToolResultCompaction,
    )
}

/// Persist the full-context auto-compaction master switch under `compaction`.
/// Unknown sibling keys (routing, breaker, experimental) survive the write.
pub(crate) fn save_compaction_enabled(cwd: &Path, enabled: bool) -> Result<()> {
    let project = project_path(cwd);
    update_global_nested(
        &["compaction"],
        &[("enabled", Value::Bool(enabled))],
        SaveValidation::CompactionTrigger {
            project: Some(&project),
        },
    )
}

/// Persist the reactive deterministic-recovery toggle under `compaction`.
pub(crate) fn save_compaction_reactive(cwd: &Path, enabled: bool) -> Result<()> {
    let project = project_path(cwd);
    update_global_nested(
        &["compaction"],
        &[("reactive", Value::Bool(enabled))],
        SaveValidation::CompactionTrigger {
            project: Some(&project),
        },
    )
}

/// Persist the protected-tail size under `compaction.keepRecentTokens`, clamped
/// to the same positive token range as the other budget dials.
pub(crate) fn save_compaction_keep_recent_tokens(cwd: &Path, tokens: u64) -> Result<()> {
    let tokens = tokens.clamp(MIN_CONTEXT_TOKEN_BUDGET, MAX_CONTEXT_TOKEN_BUDGET);
    let project = project_path(cwd);
    update_global_nested(
        &["compaction"],
        &[("keepRecentTokens", Value::from(tokens))],
        SaveValidation::CompactionTrigger {
            project: Some(&project),
        },
    )
}

/// Persist the hard-tier bounded wait under `compaction.hardWaitMs`, clamped to
/// [`MAX_COMPACTION_HARD_WAIT_MS`] at the boundary so a hand-typed dial value can
/// never request a wait longer than the harness accepts. The `CompactionTrigger`
/// re-check folds in any project override before the write reaches disk.
pub(crate) fn save_compaction_hard_wait(cwd: &Path, ms: u64) -> Result<()> {
    let ms = ms.min(MAX_COMPACTION_HARD_WAIT_MS);
    let project = project_path(cwd);
    update_global_nested(
        &["compaction"],
        &[("hardWaitMs", Value::from(ms))],
        SaveValidation::CompactionTrigger {
            project: Some(&project),
        },
    )
}

/// Persist one auto-compaction threshold fraction under `compaction.thresholds`.
/// The `CompactionTrigger` re-check rejects any write that would make the merged
/// ladder unordered (`0 < warn < start < hard < 1`) before it reaches disk.
fn save_compaction_threshold(cwd: &Path, key: &'static str, fraction: f64) -> Result<()> {
    let number = serde_json::Number::from_f64(fraction)
        .context("compaction threshold must be a finite fraction")?;
    let project = project_path(cwd);
    update_global_nested(
        &["compaction", "thresholds"],
        &[(key, Value::Number(number))],
        SaveValidation::CompactionTrigger {
            project: Some(&project),
        },
    )
}

pub(crate) fn save_compaction_threshold_warn(cwd: &Path, fraction: f64) -> Result<()> {
    save_compaction_threshold(cwd, "warn", fraction)
}

pub(crate) fn save_compaction_threshold_start(cwd: &Path, fraction: f64) -> Result<()> {
    save_compaction_threshold(cwd, "start", fraction)
}

pub(crate) fn save_compaction_threshold_hard(cwd: &Path, fraction: f64) -> Result<()> {
    save_compaction_threshold(cwd, "hard", fraction)
}

/// Persist the background worker's input mode under `compaction.worker.input`.
/// An unrecognized value falls back to `transcript` (the built-in default), the
/// same way the load-time accessor degrades.
pub(crate) fn save_compaction_worker_input(input: &str) -> Result<()> {
    let input = match input {
        "transcript" | "investigator" => input,
        _ => "transcript",
    };
    update_global_nested(
        &["compaction", "worker"],
        &[("input", Value::String(input.to_string()))],
        SaveValidation::None,
    )
}

/// Persist the durable task workflow toggle in the project settings file for
/// `cwd`. This is intentionally project-scoped: teams may opt a repository into
/// the review/rollback workflow. The separate mutation-safety master remains
/// global-only.
pub(crate) fn save_project_tasks(cwd: &Path, enabled: bool) -> Result<()> {
    update_project(cwd, &[("tasks", Value::Bool(enabled))])
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

/// Apply top-level updates to this workspace's project settings file,
/// preserving unknown keys exactly like [`update_global`]. Used only for
/// project-safe knobs.
fn update_project(cwd: &Path, updates: &[(&str, Value)]) -> Result<()> {
    let path = project_path(cwd);
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
    // Clearing a block's last key removes the block: a settings file never
    // accumulates empty `{}` residue from cleared values.
    if nested.is_empty() {
        object.remove(block);
    } else {
        object.insert(block.to_string(), Value::Object(nested));
    }
    write_object_atomically(&path, &object)
}

/// Which policy invariant a nested settings write must re-check against the
/// merged on-disk object before it is allowed to reach disk. Keeps invalid
/// combinations (an incompatible tool-result policy, an unordered auto-compaction
/// ladder) out of both the file and the live harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveValidation<'a> {
    /// No cross-field re-check (the value is already bounded at the boundary).
    None,
    /// Re-parse and re-resolve the structured tool-result-compaction policy.
    ToolResultCompaction,
    /// Re-validate the auto-compaction trigger ladder (`0 < warn < start < hard
    /// < 1`, positive tail) against the MERGED global+project settings the
    /// harness loads. `project` is the project settings path whose overrides
    /// must be folded in before the check; `None` (no project context) keeps the
    /// global object as the merged result.
    CompactionTrigger { project: Option<&'a Path> },
}

fn update_global_nested(
    blocks: &[&str],
    updates: &[(&str, Value)],
    validation: SaveValidation<'_>,
) -> Result<()> {
    let path = global_path()
        .context("cannot resolve the global settings path (set HOME or IRIS_CONFIG_PATH)")?;
    let mut object = read_object(&path)?;
    let mut current = &mut object;
    for block in blocks {
        let value = current
            .entry((*block).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !value.is_object() {
            *value = Value::Object(Map::new());
        }
        current = value
            .as_object_mut()
            .expect("value replaced with an object above");
    }
    for (key, value) in updates {
        if value.is_null() {
            current.remove(*key);
        } else {
            current.insert((*key).to_string(), value.clone());
        }
    }
    match validation {
        SaveValidation::None => {}
        SaveValidation::ToolResultCompaction => {
            let settings: Settings = serde_json::from_value(Value::Object(object.clone()))
                .context("updated settings are invalid")?;
            let policy = settings.tool_result_compaction()?;
            if policy.enabled {
                crate::mimir::selection::ModelSelection::resolve(&settings)?;
            }
        }
        SaveValidation::CompactionTrigger { project } => {
            let settings: Settings = serde_json::from_value(Value::Object(object.clone()))
                .context("updated settings are invalid")?;
            // Rejects an unordered/degenerate ladder before it can reach disk or
            // the harness. The check runs against the MERGED global+project
            // ladder the harness actually loads, because warn/start/hard/
            // keepRecentTokens/enabled/reactive are project-overridable: a
            // globally-valid write can still yield an invalid merged ladder.
            validate_merged_compaction_ladder(settings, project)?;
        }
    }
    write_object_atomically(&path, &object)
}

/// Validate the auto-compaction ladder against the merged global+project
/// settings the harness loads at [`Settings::load`], not the global object
/// alone. warn/start/hard/keepRecentTokens/enabled/reactive are
/// project-overridable by design, so a globally-valid write can still produce an
/// invalid merged ladder (e.g. a project `start` above a freshly-saved global
/// `hard`). Reject that before the global write reaches disk and name the
/// conflicting project override. With no project file (or no `compaction`
/// block) the merged result equals the global object, preserving the prior
/// global-only behavior.
fn validate_merged_compaction_ladder(global: Settings, project: Option<&Path>) -> Result<()> {
    let Some((path, project_settings)) = project
        .map(|path| read_optional(path).map(|settings| (path, settings)))
        .transpose()?
        .and_then(|(path, settings)| settings.map(|settings| (path, settings)))
    else {
        // No project context: the global object is the merged result.
        global.compaction_trigger()?;
        return Ok(());
    };
    let overrides = project_compaction_overrides(project_settings.compaction.as_ref());
    if let Err(error) = global.merged_with(project_settings).compaction_trigger() {
        if overrides.is_empty() {
            return Err(error);
        }
        bail!(
            "cannot save: applying the project settings at {} makes the merged \
             auto-compaction ladder invalid (project overrides {}): {error:#}",
            path.display(),
            overrides.join(", "),
        );
    }
    Ok(())
}

/// The auto-compaction keys a project settings file overrides, named for the
/// rejection message so the operator knows which project value collides with the
/// global write.
fn project_compaction_overrides(project: Option<&CompactionSettings>) -> Vec<&'static str> {
    let mut keys = Vec::new();
    let Some(compaction) = project else {
        return keys;
    };
    if let Some(thresholds) = compaction.thresholds.as_ref() {
        if thresholds.warn.is_some() {
            keys.push("compaction.thresholds.warn");
        }
        if thresholds.start.is_some() {
            keys.push("compaction.thresholds.start");
        }
        if thresholds.hard.is_some() {
            keys.push("compaction.thresholds.hard");
        }
    }
    if compaction.keep_recent_tokens.is_some() {
        keys.push("compaction.keepRecentTokens");
    }
    if compaction.enabled.is_some() {
        keys.push("compaction.enabled");
    }
    if compaction.reactive.is_some() {
        keys.push("compaction.reactive");
    }
    keys
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
    // A test that has not opted into a real path via `ConfigPathGuard` /
    // `IRIS_CONFIG_PATH` must NEVER be ABLE to touch the developer's real
    // settings file: persisting code paths (model-switch defaults, login,
    // scoped saves) run inside unit tests, and without a guard they would
    // fall through to the real `$HOME` — silently rewriting the settings of
    // whoever runs the suite. Unguarded reads and writes land in a
    // process-scoped scratch file instead; tests asserting on file content
    // keep opting in through the guard.
    #[cfg(test)]
    {
        Some(std::env::temp_dir().join(format!("iris-test-settings-{}.json", std::process::id())))
    }
    #[cfg(not(test))]
    {
        let home = env::var("HOME").ok().filter(|home| !home.is_empty())?;
        Some(Path::new(&home).join(".iris/settings.json"))
    }
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
    use crate::mimir::test_support::ConfigPathGuard;
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

    struct EnvVarGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let prev = env::var(key).ok();
            // SAFETY: tests that mutate process env hold the shared mimir env
            // lock through ConfigPathGuard / env_lock. This guard is declared
            // after that lock holder in the test, so it restores before the
            // lock is released.
            unsafe { env::remove_var(key) };
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(prev) => unsafe { env::set_var(self.key, prev) },
                None => unsafe { env::remove_var(self.key) },
            }
        }
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
    fn tool_result_compaction_defaults_off_and_legacy_alias_is_conservative() {
        let defaulted = Settings::default().tool_result_compaction().unwrap();
        assert!(!defaulted.enabled);
        assert_eq!(
            defaulted.aggressiveness,
            CompactionAggressiveness::Conservative
        );
        assert_eq!(defaulted.cache_timing, CompactionCacheTiming::CacheAware);
        assert_eq!(defaulted.trigger_tokens, 64_000);
        assert!(defaulted.semantic_dedupe.enabled);
        assert_eq!(defaulted.semantic_dedupe.retain_per_path, 1);
        assert!(!defaulted.tool_clearing.enabled);
        assert!(defaulted.legacy_alias);

        let legacy = Settings {
            microcompaction: Some(true),
            microcompaction_watermark: Some(17_000),
            ..Settings::default()
        }
        .tool_result_compaction()
        .unwrap();
        assert!(legacy.enabled);
        assert_eq!(legacy.trigger_tokens, 17_000);
        assert!(legacy.semantic_dedupe.enabled);
        assert!(!legacy.tool_clearing.enabled);
    }

    #[test]
    fn structured_compaction_parses_presets_and_explicit_overrides() {
        let raw = serde_json::json!({
            "toolResultCompaction": {
                "enabled": true,
                "aggressiveness": "aggressive",
                "cacheTiming": "immediate",
                "triggerTokens": 12000,
                "semanticDedupe": {
                    "retainPerPath": 3,
                    "protectRecentToolResults": 7,
                    "protectRecentTokens": 900
                },
                "toolClearing": {
                    "backend": "local",
                    "mode": "selected",
                    "keepRecentToolUses": 5,
                    "clearAtLeastTokens": 200,
                    "eligibleTools": ["bash", "grep", "bash"],
                    "excludedTools": ["edit"],
                    "includeFailures": true
                }
            }
        });
        let settings: Settings = serde_json::from_value(raw).unwrap();
        let policy = settings.tool_result_compaction().unwrap();
        assert!(policy.enabled);
        assert_eq!(policy.cache_timing, CompactionCacheTiming::Immediate);
        assert_eq!(policy.semantic_dedupe.retain_per_path, 3);
        assert_eq!(policy.semantic_dedupe.protect_recent_tool_results, 7);
        assert!(policy.tool_clearing.enabled, "aggressive preset enables B");
        assert_eq!(policy.tool_clearing.mode, ToolClearingMode::Selected);
        assert_eq!(policy.tool_clearing.eligible_tools, vec!["bash", "grep"]);
        assert!(policy.tool_clearing.include_failures);
        assert!(!policy.legacy_alias);
    }

    #[test]
    fn structured_compaction_rejects_degenerate_counts_and_empty_names() {
        for (raw, needle) in [
            (
                serde_json::json!({"toolResultCompaction":{"enabled":true,"triggerTokens":0}}),
                "triggerTokens",
            ),
            (
                serde_json::json!({"toolResultCompaction":{"enabled":true,"semanticDedupe":{"retainPerPath":0}}}),
                "retainPerPath",
            ),
            (
                serde_json::json!({"toolResultCompaction":{"enabled":true,"semanticDedupe":{"protectRecentToolResults":0,"protectRecentTokens":0}}}),
                "recent working set",
            ),
            (
                serde_json::json!({"toolResultCompaction":{"enabled":true,"aggressiveness":"custom","toolClearing":{"enabled":true,"mode":"selected","eligibleTools":[]}}}),
                "eligibleTools",
            ),
            (
                serde_json::json!({"toolResultCompaction":{"enabled":true,"aggressiveness":"custom","toolClearing":{"enabled":true,"mode":"selected","eligibleTools":["  "]}}}),
                "empty tool name",
            ),
        ] {
            let settings: Settings = serde_json::from_value(raw).unwrap();
            let error = format!("{:#}", settings.tool_result_compaction().unwrap_err());
            assert!(
                error.contains(needle),
                "{error:?} did not contain {needle:?}"
            );
        }
    }

    #[test]
    fn project_merge_tunes_local_policy_but_cannot_select_or_rewrite_native() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{"toolResultCompaction":{"enabled":true,"toolClearing":{"enabled":true,"backend":"anthropicNative","keepRecentToolUses":4,"excludedTools":["read","ls"]}}}"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"{"toolResultCompaction":{"enabled":false,"cacheTiming":"pressureOnly","semanticDedupe":{"retainPerPath":2},"toolClearing":{"backend":"local","keepRecentToolUses":99}}}"#,
        )
        .unwrap();
        let merged = Settings::load_from(Some(&global), &project).unwrap();
        let policy = merged.tool_result_compaction().unwrap();
        assert!(
            policy.enabled,
            "project cannot disable global native config"
        );
        assert_eq!(policy.cache_timing, CompactionCacheTiming::PressureOnly);
        assert_eq!(policy.semantic_dedupe.retain_per_path, 2);
        assert_eq!(
            policy.tool_clearing.backend,
            ToolClearingBackend::AnthropicNative
        );
        assert_eq!(policy.tool_clearing.keep_recent_tool_uses, 4);

        fs::write(&global, "{}").unwrap();
        fs::write(
            &project,
            r#"{"toolResultCompaction":{"enabled":true,"toolClearing":{"enabled":true,"backend":"auto"}}}"#,
        )
        .unwrap();
        let merged = Settings::load_from(Some(&global), &project).unwrap();
        let policy = merged.tool_result_compaction().unwrap();
        assert!(policy.enabled);
        assert!(
            !policy.tool_clearing.enabled,
            "project native block ignored"
        );
        assert_eq!(policy.tool_clearing.backend, ToolClearingBackend::Local);
    }

    #[test]
    fn structured_compaction_save_helpers_preserve_unknown_nested_keys() {
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        fs::write(
            &path,
            r#"{"futureTop":9,"toolResultCompaction":{"futurePolicy":7,"semanticDedupe":{"futureSemantic":3},"toolClearing":{"futureClearing":4}}}"#,
        )
        .unwrap();
        let _guard = ConfigPathGuard::set(&path);

        save_tool_result_compaction_aggressiveness("balanced").unwrap();
        save_tool_result_compaction_cache_timing("pressureOnly").unwrap();
        save_tool_result_compaction_trigger_tokens(12_000).unwrap();
        save_tool_result_compaction_retain_per_path(3).unwrap();
        save_tool_result_compaction_keep_recent_tool_uses(6).unwrap();
        save_tool_result_compaction_enabled(true).unwrap();

        let object = read_object(&path).unwrap();
        assert_eq!(object["futureTop"], 9);
        let policy = &object["toolResultCompaction"];
        assert_eq!(policy["futurePolicy"], 7);
        assert_eq!(policy["aggressiveness"], "balanced");
        assert_eq!(policy["cacheTiming"], "pressureOnly");
        assert_eq!(policy["triggerTokens"], 12_000);
        assert_eq!(policy["semanticDedupe"]["retainPerPath"], 3);
        assert_eq!(policy["semanticDedupe"]["futureSemantic"], 3);
        assert_eq!(policy["toolClearing"]["keepRecentToolUses"], 6);
        assert_eq!(policy["toolClearing"]["futureClearing"], 4);
        let loaded = Settings::load_from(Some(&path), &dir.path.join("none.json")).unwrap();
        assert!(loaded.tool_result_compaction().unwrap().enabled);
    }

    #[test]
    fn auto_compaction_save_helpers_preserve_unknown_nested_keys() {
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        // A hand-authored file carrying a service-hatch-only key this section
        // does not surface (worker.model), a pre-existing hardWaitMs the called
        // helpers must leave untouched, plus future siblings.
        fs::write(
            &path,
            r#"{"futureTop":9,"compaction":{"futurePolicy":7,"hardWaitMs":5000,"worker":{"model":"anthropic/claude-opus-4-6","futureWorker":3},"thresholds":{"futureThreshold":1}}}"#,
        )
        .unwrap();
        let _guard = ConfigPathGuard::set(&path);

        save_compaction_enabled(&dir.path, true).unwrap();
        save_compaction_threshold_warn(&dir.path, 0.20).unwrap();
        save_compaction_threshold_start(&dir.path, 0.30).unwrap();
        save_compaction_threshold_hard(&dir.path, 0.40).unwrap();
        save_compaction_keep_recent_tokens(&dir.path, 6_000).unwrap();
        save_compaction_reactive(&dir.path, false).unwrap();
        save_compaction_worker_input("investigator").unwrap();

        let object = read_object(&path).unwrap();
        assert_eq!(object["futureTop"], 9);
        let compaction = &object["compaction"];
        // Unknown siblings and the service-hatch-only keys survive untouched.
        assert_eq!(compaction["futurePolicy"], 7);
        assert_eq!(compaction["hardWaitMs"], 5000);
        assert_eq!(compaction["worker"]["model"], "anthropic/claude-opus-4-6");
        assert_eq!(compaction["worker"]["futureWorker"], 3);
        assert_eq!(compaction["thresholds"]["futureThreshold"], 1);
        // The persisted new values.
        assert_eq!(compaction["enabled"], true);
        assert_eq!(compaction["thresholds"]["warn"], 0.20);
        assert_eq!(compaction["thresholds"]["start"], 0.30);
        assert_eq!(compaction["thresholds"]["hard"], 0.40);
        assert_eq!(compaction["keepRecentTokens"], 6_000);
        assert_eq!(compaction["reactive"], false);
        assert_eq!(compaction["worker"]["input"], "investigator");

        let trigger = Settings::load_from(Some(&path), &dir.path.join("none.json"))
            .unwrap()
            .compaction_trigger()
            .unwrap();
        assert!(trigger.enabled);
        assert_eq!(trigger.warn, 0.20);
        assert_eq!(trigger.keep_recent_tokens, 6_000);
        assert!(!trigger.reactive);
    }

    #[test]
    fn save_compaction_hard_wait_persists_and_clamps_to_the_max() {
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        let _guard = ConfigPathGuard::set(&path);

        // An in-range write round-trips exactly through the loader.
        save_compaction_hard_wait(&dir.path, 90_000).unwrap();
        assert_eq!(
            read_object(&path).unwrap()["compaction"]["hardWaitMs"],
            90_000
        );
        assert_eq!(
            Settings::load_from(Some(&path), &dir.path.join("none.json"))
                .unwrap()
                .compaction_trigger()
                .unwrap()
                .hard_wait_ms,
            90_000
        );

        // A value past the 300000 ms cap is clamped at the boundary so it can
        // never reach disk (or the CompactionTrigger re-check) out of range.
        save_compaction_hard_wait(&dir.path, 10_000_000).unwrap();
        assert_eq!(
            read_object(&path).unwrap()["compaction"]["hardWaitMs"],
            MAX_COMPACTION_HARD_WAIT_MS
        );
    }

    #[test]
    fn unordered_thresholds_are_rejected_and_never_reach_disk() {
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        // A valid starting ladder (defaults: warn .60 / start .72 / hard .90).
        fs::write(&path, r#"{"compaction":{"thresholds":{"warn":0.6}}}"#).unwrap();
        let _guard = ConfigPathGuard::set(&path);

        // warn must stay below start (.72): 0.80 is unordered and must fail.
        let error = save_compaction_threshold_warn(&dir.path, 0.80).unwrap_err();
        assert!(format!("{error:#}").contains("warn < start"), "{error:#}");
        // The rejected write never persisted: the file still holds warn 0.6.
        let object = read_object(&path).unwrap();
        assert_eq!(object["compaction"]["thresholds"]["warn"], 0.6);

        // An ordered move is accepted.
        save_compaction_threshold_warn(&dir.path, 0.50).unwrap();
        assert_eq!(
            read_object(&path).unwrap()["compaction"]["thresholds"]["warn"],
            0.5
        );
    }

    #[test]
    fn global_compaction_save_rejected_when_a_project_override_invalidates_the_merged_ladder() {
        let dir = temp_dir();
        let global = dir.path.join("settings.json");
        // A globally-valid starting ladder (warn .60 < start .72).
        fs::write(
            &global,
            r#"{"compaction":{"thresholds":{"warn":0.60,"start":0.72}}}"#,
        )
        .unwrap();
        let _guard = ConfigPathGuard::set(&global);

        // A project override sets start .85, above the global hard we save next.
        let project = dir.path.join("proj");
        fs::create_dir_all(project.join(".iris")).unwrap();
        fs::write(
            project.join(".iris/settings.json"),
            r#"{"compaction":{"thresholds":{"start":0.85}}}"#,
        )
        .unwrap();

        // Global hard .80 passes the global-only check (.60 < .72 < .80) but the
        // merged ladder has start(.85) >= hard(.80): the save must be rejected.
        let error = save_compaction_threshold_hard(&project, 0.80).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("compaction.thresholds.start"),
            "error names the conflicting project override: {message}"
        );
        assert!(
            message.contains(".iris/settings.json"),
            "error names the project settings path: {message}"
        );

        // The rejected write never reached disk: global hard was never written.
        let object = read_object(&global).unwrap();
        assert!(
            object["compaction"]["thresholds"].get("hard").is_none(),
            "a rejected save must leave the global file unchanged"
        );
    }

    #[test]
    fn global_compaction_save_succeeds_without_a_conflicting_project_override() {
        let dir = temp_dir();
        let global = dir.path.join("settings.json");
        fs::write(
            &global,
            r#"{"compaction":{"thresholds":{"warn":0.60,"start":0.72}}}"#,
        )
        .unwrap();
        let _guard = ConfigPathGuard::set(&global);

        // A project file that does not touch the compaction ladder.
        let project = dir.path.join("proj");
        fs::create_dir_all(project.join(".iris")).unwrap();
        fs::write(
            project.join(".iris/settings.json"),
            r#"{"defaultModel":"project-model"}"#,
        )
        .unwrap();

        // The same global hard .80 is valid in the merged ladder and persists.
        save_compaction_threshold_hard(&project, 0.80).unwrap();
        let object = read_object(&global).unwrap();
        assert_eq!(object["compaction"]["thresholds"]["hard"], 0.80);
    }

    #[test]
    fn a_project_may_tune_the_auto_compaction_ladder_over_global() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{"compaction":{"enabled":true,"thresholds":{"warn":0.5,"start":0.6,"hard":0.7},"keepRecentTokens":4000}}"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"{"compaction":{"thresholds":{"warn":0.55},"keepRecentTokens":9000,"worker":{"input":"investigator"}}}"#,
        )
        .unwrap();
        let merged = Settings::load_from(Some(&global), &project).unwrap();
        let trigger = merged.compaction_trigger().unwrap();
        assert_eq!(trigger.warn, 0.55, "project warn wins");
        assert_eq!(trigger.start, 0.6, "global start survives");
        assert_eq!(trigger.keep_recent_tokens, 9000, "project tail wins");
        let worker = merged.compaction_worker_config().unwrap();
        assert!(matches!(
            worker.input,
            crate::wayland::CompactionWorkerInput::Investigator
        ));
    }

    #[test]
    fn task_workflow_defaults_off_and_a_project_may_opt_in() {
        assert!(!Settings::default().tasks());

        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "tasks": false }"#).unwrap();
        fs::write(&project, r#"{ "tasks": true }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert!(settings.tasks());

        save_project_tasks(&dir.path, true).unwrap();
        let saved = Settings::load_from(None, &project_path(&dir.path)).unwrap();
        assert!(saved.tasks());
    }

    #[test]
    fn bash_tool_mode_defaults_off_and_a_project_may_tune_it() {
        // Default off: an unset key keeps the full built-in tool surface.
        assert!(!Settings::default().bash_tool_mode());

        // Tool-surface knob, not a security redirect: a project may turn it on.
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, "{}").unwrap();
        fs::write(&project, r#"{ "bashToolMode": true }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert!(settings.bash_tool_mode());
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
    fn compaction_trigger_defaults_and_validates_legacy_off_switch() {
        let defaults = Settings::default().compaction_trigger().unwrap();
        assert!(defaults.enabled);
        assert_eq!(
            (defaults.warn, defaults.start, defaults.hard),
            (0.60, 0.72, 0.90)
        );
        assert_eq!(defaults.keep_recent_tokens, 8_000);
        assert_eq!(defaults.hard_wait_ms, 120_000);
        assert_eq!(defaults.max_consecutive_failures, 3);
        assert!(defaults.reactive);

        let too_small = Settings {
            context_token_budget: Some(8_191),
            ..Settings::default()
        };
        let error = too_small.compaction_trigger().unwrap_err().to_string();
        assert!(error.contains("compaction.enabled=false"), "{error}");

        let reactive_off = Settings {
            compaction: Some(CompactionSettings {
                reactive: Some(false),
                ..CompactionSettings::default()
            }),
            ..Settings::default()
        };
        assert!(!reactive_off.compaction_trigger().unwrap().reactive);
    }

    #[test]
    fn compaction_thresholds_must_be_ordered_and_project_knobs_merge() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{
              "compaction": {
                "thresholds": { "warn": 0.50, "start": 0.60, "hard": 0.80 },
                "worker": { "model": "anthropic/claude-haiku-4-5", "timeoutMs": 90000 },
                "providerNative": "auto"
              }
            }"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"{
              "compaction": {
                "enabled": false,
                "thresholds": { "start": 0.62 },
                "keepRecentTokens": 12000,
                "worker": { "model": "evil/redirect", "timeoutMs": 1000 },
                "providerNative": "off"
              }
            }"#,
        )
        .unwrap();
        let merged = Settings::load_from(Some(&global), &project).unwrap();
        let trigger = merged.compaction_trigger().unwrap();
        assert_eq!(
            (trigger.warn, trigger.start, trigger.hard),
            (0.50, 0.62, 0.80)
        );
        assert!(!trigger.enabled);
        assert_eq!(trigger.keep_recent_tokens, 12_000);
        let block = merged.compaction.as_ref().unwrap();
        assert_eq!(
            block.worker.as_ref().unwrap().model.as_deref(),
            Some("anthropic/claude-haiku-4-5"),
            "project config cannot redirect worker traffic"
        );
        assert_eq!(block.provider_native.as_deref(), Some("auto"));
        assert_eq!(
            merged.compaction_provider_native().unwrap(),
            ProviderNativeMode::Auto
        );

        let worker = merged.compaction_worker_config().unwrap();
        assert_eq!(
            worker.input,
            crate::wayland::CompactionWorkerInput::Transcript
        );
        assert_eq!(worker.timeout, std::time::Duration::from_millis(1_000));
        assert_eq!(
            merged.compaction_worker_model(),
            Some("anthropic/claude-haiku-4-5")
        );

        let invalid = Settings {
            compaction: Some(CompactionSettings {
                thresholds: Some(CompactionThresholdSettings {
                    warn: Some(0.7),
                    start: Some(0.6),
                    hard: Some(0.8),
                }),
                ..CompactionSettings::default()
            }),
            ..Settings::default()
        };
        assert!(invalid.compaction_trigger().is_err());

        let invalid_native = Settings {
            compaction: Some(CompactionSettings {
                provider_native: Some("always".to_string()),
                ..CompactionSettings::default()
            }),
            ..Settings::default()
        };
        assert!(
            invalid_native
                .compaction_provider_native()
                .unwrap_err()
                .to_string()
                .contains("off|auto")
        );

        // The new cap is 300000 ms (5 min); one past it is rejected, exactly at
        // it is accepted.
        let unbounded_wait = Settings {
            compaction: Some(CompactionSettings {
                hard_wait_ms: Some(300_001),
                ..CompactionSettings::default()
            }),
            ..Settings::default()
        };
        let error = unbounded_wait.compaction_trigger().unwrap_err().to_string();
        assert!(error.contains("at most 300000"), "{error}");

        let at_cap = Settings {
            compaction: Some(CompactionSettings {
                hard_wait_ms: Some(300_000),
                ..CompactionSettings::default()
            }),
            ..Settings::default()
        };
        assert_eq!(at_cap.compaction_trigger().unwrap().hard_wait_ms, 300_000);
    }

    #[test]
    fn provider_native_defaults_auto_and_is_global_only() {
        assert_eq!(
            Settings::default().compaction_provider_native().unwrap(),
            ProviderNativeMode::Auto
        );

        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "compaction": { "providerNative": "auto" } }"#).unwrap();
        fs::write(&project, r#"{ "compaction": { "providerNative": "off" } }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(
            settings.compaction_provider_native().unwrap(),
            ProviderNativeMode::Auto
        );
    }

    #[test]
    fn compaction_model_tool_defaults_off_and_project_may_enable_it() {
        assert!(!Settings::default().compaction_model_tool());

        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "compaction": { "modelTool": false } }"#).unwrap();
        fs::write(&project, r#"{ "compaction": { "modelTool": true } }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert!(settings.compaction_model_tool());
    }

    #[test]
    fn compaction_worker_defaults_to_transcript_and_validates_input() {
        let defaults = Settings::default().compaction_worker_config().unwrap();
        assert_eq!(
            defaults.input,
            crate::wayland::CompactionWorkerInput::Transcript
        );
        assert_eq!(defaults.max_tool_roundtrips, 4);
        assert_eq!(defaults.timeout, std::time::Duration::from_millis(120_000));
        assert!(defaults.instructions.is_empty());

        let invalid = Settings {
            compaction: Some(CompactionSettings {
                worker: Some(CompactionWorkerSettings {
                    input: Some("opaque".to_string()),
                    ..CompactionWorkerSettings::default()
                }),
                ..CompactionSettings::default()
            }),
            ..Settings::default()
        };
        assert!(
            invalid
                .compaction_worker_config()
                .unwrap_err()
                .to_string()
                .contains("transcript|investigator")
        );
    }

    #[test]
    fn microcompaction_watermark_defaults_independently_and_parses_when_present() {
        let dir = temp_dir();
        let defaulted = Settings::load_from(
            Some(&dir.path.join("none.json")),
            &dir.path.join("none.json"),
        )
        .unwrap();
        assert_eq!(defaulted.microcompaction_watermark, None);
        assert_eq!(
            defaulted.microcompaction_watermark(),
            DEFAULT_MICROCOMPACTION_WATERMARK
        );

        let project = dir.path.join("project.json");
        fs::write(
            &project,
            r#"{ "contextTokenBudget": 200000, "microcompactionWatermark": 12000 }"#,
        )
        .unwrap();
        let configured = Settings::load_from(None, &project).unwrap();
        assert_eq!(configured.context_token_budget(), 200_000);
        assert_eq!(configured.microcompaction_watermark, Some(12_000));
        assert_eq!(configured.microcompaction_watermark(), 12_000);
    }

    #[test]
    fn compaction_summarizer_defaults_to_provider_and_accepts_explicit_modes() {
        let dir = temp_dir();
        let defaulted = Settings::load_from(
            Some(&dir.path.join("none.json")),
            &dir.path.join("none.json"),
        )
        .unwrap();
        assert_eq!(
            defaulted.compaction_summarizer(),
            crate::wayland::SummarizerKind::Provider
        );

        let project = dir.path.join("project.json");
        fs::write(&project, r#"{ "compactionSummarizer": "provider" }"#).unwrap();
        let settings = Settings::load_from(None, &project).unwrap();
        assert_eq!(
            settings.compaction_summarizer(),
            crate::wayland::SummarizerKind::Provider
        );

        fs::write(&project, r#"{ "compactionSummarizer": "subagent" }"#).unwrap();
        let settings = Settings::load_from(None, &project).unwrap();
        assert_eq!(
            settings.compaction_summarizer(),
            crate::wayland::SummarizerKind::Subagent
        );
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
    fn dangerous_default_approval_is_global_only() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        std::fs::write(
            &global,
            r#"{ "defaultApproval": "dangerously-skip-permissions", "dangerouslySkipPermissions": true }"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{ "defaultApproval": "dangerously-skip-permissions", "dangerouslySkipPermissions": true }"#,
        )
        .unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(
            settings.default_approval.as_deref(),
            Some("dangerously-skip-permissions"),
            "the global defaultApproval token may select dangerous mode"
        );

        let absent_global =
            Settings::load_from(Some(&dir.path.join("none.json")), &project).unwrap();
        assert_eq!(
            absent_global.default_approval, None,
            "a project file still cannot enable dangerous mode"
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
        // A cloned project must never lower the posture or enable dangerous skip.
        fs::write(
            &project,
            r#"{ "defaultApproval": "dangerously-skip-permissions" }"#,
        )
        .unwrap();
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

    #[test]
    fn save_microcompaction_rejects_enabling_beside_clear_tool_uses() {
        // Overlap rejection at the /settings save boundary: legacy
        // microcompaction targets read/ls, so native clearing must exclude both.
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        fs::write(
            &path,
            r#"{ "anthropicContextManagement": { "clearToolUses": { "triggerInputTokens": 50000 } } }"#,
        )
        .unwrap();
        let _guard = ConfigPathGuard::set(&path);

        let error = format!("{:#}", save_microcompaction(true).unwrap_err());
        assert!(error.contains("clearToolUses"), "names the edit: {error}");
        assert!(
            error.contains("microcompaction"),
            "names the toggle: {error}"
        );
        // Disabling is always allowed (it resolves the conflict).
        save_microcompaction(false).unwrap();

        fs::write(
            &path,
            r#"{ "anthropicContextManagement": { "clearToolUses": { "excludeTools": ["read", "ls"] } } }"#,
        )
        .unwrap();
        save_microcompaction(true).unwrap();

        // clearThinking alone does not block enabling.
        fs::write(
            &path,
            r#"{ "anthropicContextManagement": { "clearThinking": { "triggerInputTokens": 50000 } } }"#,
        )
        .unwrap();
        save_microcompaction(true).unwrap();
        let settings = Settings::load_from(Some(&path), &dir.path.join("none.json")).unwrap();
        assert!(settings.microcompaction());
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
        let error = save_context_token_budget(1).unwrap_err().to_string();
        assert!(error.contains("compaction.enabled=false"), "{error}");
        save_context_token_budget(DEFAULT_SUMMARY_RESERVE).unwrap();
        save_microcompaction_watermark(999_999_999).unwrap();
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
        assert_eq!(
            object.get("contextTokenBudget"),
            Some(&Value::from(DEFAULT_SUMMARY_RESERVE))
        );
        assert_eq!(
            object.get("microcompactionWatermark"),
            Some(&Value::from(100_000_000))
        );
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
            r#"{ "worktreeRoot": "../wt", "verify": { "command": "x", "maxAttempts": 2 } }"#,
        )
        .unwrap();
        let _guard = ConfigPathGuard::set(&path);

        save_worktree_root(None).unwrap();
        save_verify_command(None).unwrap();

        let object = read_object(&path).unwrap();
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
    fn clearing_a_blocks_last_key_removes_the_block() {
        // No `{}` residue: a settings file a user cleared through the panel
        // reads as if the value was never set.
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        fs::write(&path, r#"{ "verify": { "command": "x" } }"#).unwrap();
        let _guard = ConfigPathGuard::set(&path);
        save_verify_command(None).unwrap();
        let object = read_object(&path).unwrap();
        assert!(
            !object.contains_key("verify"),
            "emptied block removed: {object:?}"
        );
    }

    #[test]
    fn saved_default_model_and_reasoning_are_reapplied_on_restart() {
        // Issue #490: the whole point of persisting defaults is that a fresh
        // startup reuses them without reconfiguration. Prove the full loop --
        // save to disk (as `/model` + `/reasoning` do), reload the file, and
        // resolve the startup selection -- rather than only asserting the file
        // contents (which the CLI-level tests already cover).
        use crate::mimir::selection::ModelSelection;

        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        let _guard = ConfigPathGuard::set(&path);
        let _model_env = EnvVarGuard::unset("IRIS_MODEL");

        save_default_model("openai-codex", "issue-490-custom-model").unwrap();
        save_default_reasoning("high").unwrap();

        // Reload from disk exactly like startup (no project file present).
        let reloaded = Settings::load_from(Some(&path), &dir.path.join("no-project.json")).unwrap();
        let resolved = ModelSelection::resolve(&reloaded).unwrap();

        assert_eq!(resolved.provider.as_str(), "openai-codex");
        assert_eq!(resolved.model, "issue-490-custom-model");
        assert_eq!(
            resolved.reasoning.map(|level| level.as_str().to_string()),
            Some("high".to_string()),
            "persisted defaultReasoning must be reapplied at startup"
        );
    }

    #[test]
    fn mutation_safety_defaults_on_and_project_cannot_disable_it() {
        assert!(Settings::default().mutation_safety());
        let global: Settings =
            serde_json::from_str(r#"{"mutationSafety":true,"tasks":true}"#).unwrap();
        let project: Settings = serde_json::from_str(r#"{"mutationSafety":false}"#).unwrap();
        let merged = global.merged_with(project);
        assert!(merged.mutation_safety());
        assert!(merged.tasks());

        let global = Settings::default();
        let project: Settings = serde_json::from_str(r#"{"mutationSafety":false}"#).unwrap();
        assert!(global.merged_with(project).mutation_safety());
    }

    #[test]
    fn save_mutation_safety_round_trips() {
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        let _guard = ConfigPathGuard::set(&path);
        save_mutation_safety(false).unwrap();
        let loaded = Settings::load_from(Some(&path), &dir.path.join("none.json")).unwrap();
        assert!(!loaded.mutation_safety());
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
