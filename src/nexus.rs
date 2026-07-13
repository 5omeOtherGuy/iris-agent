use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::{Error, Result, anyhow, bail};
use futures::Stream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// The agent loop has no built-in fixed turn cap: it runs while the model keeps
// emitting tool calls and ends naturally when the model stops (matching
// pi-mono's `runLoop`, which has no maxTurns/maxSteps). Cancellation (Ctrl-C) is
// the always-on runaway guard. An optional soft cap can be supplied via
// `Settings` (`Agent::max_tool_roundtrips`); when set it ends the turn with a
// graceful Notice rather than a fatal error (see complete_turn).

// Oversized-tool-output policy (issue #61). A successful tool result whose text
// exceeds this many bytes is stored out of context behind a handle and replaced
// in the transcript by a compact head+tail preview, so a large output is not
// reinserted into provider context on every round-trip. Outputs at or below the
// threshold stay inline exactly as before.
//
// ponytail: fixed default. Matches pi-mono's 50 KB truncate threshold
// (`harness/utils/truncate.ts` DEFAULT_MAX_BYTES). Covers ordinary tool output
// (file reads, small greps) inline and catches genuinely large logs. Upgrade
// path = a `Settings` knob threaded through `ToolEnv` (kept a constant here to
// avoid a config field that triggers nothing, like `contextTokenBudget` already
// is). Bytes, not lines: context cost tracks bytes/tokens, not line count.
const MAX_INLINE_TOOL_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_INTERACTION_FEEDBACK_BYTES: usize = 8 * 1024;

// Head/tail kept in the compact preview of an offloaded output. Their sum is
// well under MAX_INLINE_TOOL_OUTPUT_BYTES, so an offloaded result is always
// smaller than the threshold it crossed, and head/tail never overlap (offloaded
// content is strictly larger than head+tail).
const PREVIEW_HEAD_BYTES: usize = 4 * 1024;
const PREVIEW_TAIL_BYTES: usize = 2 * 1024;

// Shared between every cancellation exit path so the front-end renders one
// consistent message whether the interrupt landed before, during, or after the
// provider stream.
const INTERRUPT_NOTICE: &str = "interrupted; send another message to continue.";

/// Loud one-time warning shown at session start when
/// `--dangerously-skip-permissions` is active (ADR-0049). ASCII only so it
/// renders identically on stderr and in the TUI; kept here as the single source
/// of truth for the host banner and the audit trail.
pub(crate) const SKIP_PERMISSIONS_BANNER: &str =
    "ALL PERMISSION CHECKS DISABLED - every command will run without approval";

/// Persisted/runtime token for the explicit approval-gate bypass mode. This is
/// the only `defaultApproval` value that maps to skip-permissions; project
/// settings are never allowed to supply it because config merging keeps
/// `defaultApproval` global-only.
pub(crate) const DANGEROUS_SKIP_PERMISSIONS_TOKEN: &str = "dangerously-skip-permissions";

/// Outcome of an approval review for a single tool call. Provider/UI-neutral so
/// the core loop owns the approval policy without depending on any front-end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    /// Allow this one call.
    Allow,
    /// Allow this call and auto-approve later calls of the same tool for the
    /// rest of the session. Nexus owns and enforces that session policy.
    AllowAlways,
    /// Allow this call and record a persistent per-project grant (ADR-0027):
    /// the tool name for a non-bash tool, or the exact command for `bash`.
    /// Nexus derives and applies the grant; a destructive call is never
    /// granted (the ADR-0010 floor).
    AllowProject,
    /// Refuse this call. Default for empty/invalid/EOF input (safe-by-default).
    Deny,
}

/// Result of a required human interaction. Unlike approval, submission carries
/// the arguments the front-end populated, while rejection can carry bounded
/// model-visible feedback when the user chooses to continue the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InteractionOutcome {
    Submitted(Value),
    Rejected { feedback: Option<String> },
}

/// Operator-selected approval preset (ADR-0032). A UX-facing preset over the
/// approval-policy axis; Nexus remains the enforcement point (ADR-0005). The
/// mode never bypasses a safety floor (destructive/dirty/sandbox): it only
/// decides what happens for a call that is NOT already blocked by a floor and
/// NOT covered by an explicit session/project grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ApprovalMode {
    /// `strict` / `on-request`: prompt for every non-allowlisted gated call.
    /// The historical default; behavior is unchanged from before ADR-0032.
    #[default]
    Strict,
    /// `auto`: additionally auto-run calls Nexus can prove safe under the
    /// deterministic auto policy (a clean, in-workspace `edit`/`write`); every
    /// other gated call still prompts. Never bypasses a floor.
    Auto,
    /// `never` / `never-ask`: never prompt. A call that would require a prompt
    /// is denied and returned to the model as a normal denied result. Explicit
    /// non-floor session/project grants still run (they are not "prompts").
    NeverAsk,
}

impl ApprovalMode {
    /// Stable lowercase token for parsing/rendering (`strict`/`auto`/`never`).
    pub(crate) fn as_token(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Auto => "auto",
            Self::NeverAsk => "never",
        }
    }

    /// Parse a user token from `/approval <mode>`. Accepts the canonical tokens
    /// plus the ADR's status-label spellings; unknown input yields `None` so
    /// the caller can show usage.
    pub(crate) fn parse(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "strict" | "on-request" | "onrequest" => Some(Self::Strict),
            "auto" => Some(Self::Auto),
            "never" | "never-ask" | "neverask" => Some(Self::NeverAsk),
            _ => None,
        }
    }
}

/// Exclusive permission mode selected by the operator. Normal approval presets
/// clear the dangerous bypass; the dangerous mode enables skip-permissions and
/// makes the normal approval preset irrelevant until a normal mode is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionMode {
    Approval(ApprovalMode),
    DangerousSkipPermissions,
}

impl PermissionMode {
    pub(crate) fn parse(token: &str) -> Option<Self> {
        let token = token.trim().to_ascii_lowercase();
        match token.as_str() {
            DANGEROUS_SKIP_PERMISSIONS_TOKEN | "--dangerously-skip-permissions" => {
                Some(Self::DangerousSkipPermissions)
            }
            _ => ApprovalMode::parse(&token).map(Self::Approval),
        }
    }

    /// Resolve the persisted global default permission mode. Invalid or absent
    /// values fall back to `strict`; project config cannot supply this field.
    pub(crate) fn from_startup_setting(setting: Option<&str>) -> Self {
        setting
            .and_then(Self::parse)
            .unwrap_or(Self::Approval(ApprovalMode::Strict))
    }
}

/// A single persistent project-policy grant (ADR-0027), derived by Nexus from
/// an approved call. Data only: how it is persisted is the host's concern (a
/// [`ProjectPolicySink`] injected at construction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PolicyGrant {
    /// Auto-approve every call of this (non-bash) tool, e.g. `write`/`edit`.
    Tool(String),
    /// Auto-approve this exact `bash` command string.
    BashExact(String),
}

/// Persists a project grant when the user chooses "always for this project".
/// Implemented by the Tier-2 store; Nexus only calls it after a deliberate
/// user decision (ADR-0014: nothing self-waives).
pub(crate) trait ProjectPolicySink {
    fn persist(&self, grant: &PolicyGrant) -> Result<()>;
}

/// The persistent per-project (per-cwd) permission policy the approval loop
/// consults as the "project" precedence layer (ADR-0027): session
/// (`session_allowed`) > project (this) > global default (prompt). All layers
/// are allow-only, so consulting them is a union; the destructive floor
/// (ADR-0010) is applied before any layer. Data only -- loaded by the host
/// from the HOME-owned store, never read from any repo-controlled file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectPolicy {
    /// Non-bash tools whose calls are auto-approved (e.g. `write`, `edit`).
    pub(crate) tools: BTreeSet<String>,
    /// Exact `bash` command strings that are auto-approved.
    pub(crate) bash_exact: BTreeSet<String>,
    /// `bash` command prefixes that are auto-approved (token-boundary match).
    pub(crate) bash_prefix: BTreeSet<String>,
}

impl ProjectPolicy {
    /// Whether this policy grants the call. `bash` is matched per command
    /// (exact or token-boundary prefix), never per tool name: a blanket bash
    /// grant is intentionally not expressible. Destructiveness is NOT checked
    /// here -- the loop applies the destructive floor before consulting any
    /// allow layer.
    fn allows(&self, name: &str, args: &Value) -> bool {
        if name == "bash" {
            let Some(command) = args.get("command").and_then(Value::as_str) else {
                return false;
            };
            let command = command.trim();
            self.bash_exact.contains(command)
                || self
                    .bash_prefix
                    .iter()
                    .any(|prefix| bash_prefix_matches(prefix, command))
        } else {
            self.tools.contains(name)
        }
    }

    /// Apply a grant to the in-memory policy (mirrors what the sink persists).
    fn apply(&mut self, grant: &PolicyGrant) {
        match grant {
            PolicyGrant::Tool(name) => {
                self.tools.insert(name.clone());
            }
            PolicyGrant::BashExact(command) => {
                self.bash_exact.insert(command.clone());
            }
        }
    }
}

/// Token-boundary prefix match for a bash command grant: `git ` (or `git`)
/// matches `git status` and `git`, but never `gitevil`. Prevents a stored
/// prefix from silently widening to lexically-adjacent commands.
fn bash_prefix_matches(prefix: &str, command: &str) -> bool {
    let prefix = prefix.trim_end();
    if prefix.is_empty() || !command.starts_with(prefix) {
        return false;
    }
    command.len() == prefix.len() || command[prefix.len()..].starts_with(char::is_whitespace)
}

/// Terminal outcome of a task's post-change verification loop (issue #265).
/// Carried by [`AgentEvent::Verification`]; every variant is honest about what
/// actually happened -- a failure is never reported as a pass, and the failing
/// output is preserved (truncated) rather than suppressed. The failed task
/// stays unsettled and rollbackable (ADR-0028): verification never settles it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerificationOutcome {
    /// The verification command passed after `attempts` run(s) (1 = first try).
    Passed { attempts: u32 },
    /// The command still failed after `attempts` run(s) (== the configured cap,
    /// or fewer when the model stopped making changes). `last_output` is the
    /// final command output, truncated to a sane size for display.
    Failed {
        attempts: u32,
        exit_code: Option<i32>,
        last_output: String,
    },
    /// The `verify` block is present but configures no command: the feature is
    /// engaged yet has nothing to run, reported honestly rather than silently.
    SkippedUnconfigured,
    /// The user denied the verification command's approval prompt, so no
    /// verification claim is made (not a pass, not silently dropped).
    SkippedApprovalDenied,
}

/// Why a microcompaction fold flush ran (issue #400, cache-aware scheduling).
/// Detection recomputes the pending fold set at every turn boundary; a flush
/// waits for one of these triggers. Classes follow the design's taxonomy:
/// `A*` = piggyback on a prefix-cache break that happens anyway (marginal
/// cache-write cost ~0), `B` = inferred-cold cache, `C` = the shipped
/// token-watermark pressure backstop. Carried on the persisted `fold` entry
/// and the [`AgentEvent::FoldApplied`] observer event; provider-neutral
/// metadata only (never affects whether or how compaction runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldTrigger {
    /// A1: a compaction will fire at this same boundary; the fold rides it.
    CompactionBoundary,
    /// A2: the model/provider selection changed since the last request (a
    /// full prefix-cache break on every lane -- caches are model-scoped).
    SelectionSwitch,
    /// A3: the reasoning-effort preference changed since the last request (a
    /// message-level break; folds live in messages, so they are covered).
    ReasoningSwitch,
    /// A4: resumed after an idle gap past the provider's cold threshold, so
    /// the prefix cache is expired and the suffix re-bills regardless.
    ColdResume,
    /// A5: the context is below the provider's minimum cacheable prefix, so
    /// nothing is cached yet and a fold breaks nothing.
    BelowMinCacheable,
    /// A6: the user requested compaction manually (`/compact`); pending folds
    /// ride the user-initiated break.
    ManualCompact,
    /// B: mid-session idle gap past the provider's cold threshold -- the
    /// cache has expired (or provably will have) with no break pending, so
    /// the next request re-bills the suffix regardless.
    InferredCold,
    /// C: the context reached the micro-watermark (pressure backstop).
    Watermark,
    /// Explicit immediate policy: pending local folds flush at every safe turn
    /// boundary even when no cache break or pressure trigger is present.
    Immediate,
}

impl FoldTrigger {
    /// The short class code recorded on fold entries and shown in the UI.
    pub(crate) fn code(self) -> &'static str {
        match self {
            FoldTrigger::CompactionBoundary => "A1",
            FoldTrigger::SelectionSwitch => "A2",
            FoldTrigger::ReasoningSwitch => "A3",
            FoldTrigger::ColdResume => "A4",
            FoldTrigger::BelowMinCacheable => "A5",
            FoldTrigger::ManualCompact => "A6",
            FoldTrigger::InferredCold => "B",
            FoldTrigger::Watermark => "C",
            FoldTrigger::Immediate => "I",
        }
    }
}

/// Context-pressure rung reached by the provider-neutral trigger ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextPressureTier {
    Normal,
    Warn,
    Start,
    Hard,
}

impl ContextPressureTier {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Warn => "warn",
            Self::Start => "start",
            Self::Hard => "hard",
        }
    }
}

/// Provenance of the context size used by the trigger ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextMeasurementSource {
    ProviderReportedPlusLocal,
    Estimated,
}

/// The semantic events the loop emits during a turn. Provider- and UI-neutral:
/// a front-end maps these onto its own rendering. Mirrors pi's `AgentEvent`
/// union (`packages/agent/src/types.ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentEvent {
    /// The measured context crossed a trigger-ladder boundary. Emitted once per
    /// crossing direction, not once per boundary evaluation.
    ContextPressure {
        tier: ContextPressureTier,
        measured: u64,
        effective_window: u64,
        source: ContextMeasurementSource,
    },
    /// One provider/model round trip is starting. The id is generated by Nexus
    /// and tags the messages/events produced by this provider response.
    ProviderTurnStarted {
        turn_id: String,
    },
    /// One provider/model round trip completed with a terminal provider event.
    /// `response_id` is the provider's opaque response id when reported; `usage`
    /// carries provider token/cache accounting. Both are safe metadata and keep
    /// provider diagnostics observable without surfacing request/response bodies.
    ProviderTurnCompleted {
        turn_id: String,
        response_id: Option<String>,
        usage: Option<ProviderUsage>,
        /// Provider-neutral reason the model turn ended, when the provider
        /// reports one. Safe metadata: an enumerated completion classification,
        /// never response text. `None` for providers that do not surface it.
        completion_reason: Option<CompletionReason>,
        /// Wall-clock timing for this provider round trip (duration and
        /// time-to-first-output). Provider-neutral measurement, not provider
        /// payload; see [`ProviderTurnTiming`].
        timing: ProviderTurnTiming,
    },
    /// One provider/model round trip was interrupted before completion.
    ProviderTurnCancelled {
        turn_id: String,
    },
    /// One provider/model round trip failed. The message is the same sanitized
    /// boundary error Iris already surfaces; provider request/response payloads
    /// and auth material are never attached to the event.
    ProviderTurnError {
        turn_id: String,
        message: String,
    },
    /// Provider-neutral tool lifecycle metadata for observability. The existing
    /// display events remain for UI compatibility; this compact event carries
    /// only ids/state and never includes tool arguments, output, provider
    /// payloads, paths, or secrets.
    ToolLifecycle {
        provider_turn_id: String,
        call_id: String,
        name: String,
        state: ToolEventState,
    },
    /// A large successful tool output was offloaded behind a session-scoped
    /// handle. Metadata only: the full body and preview are intentionally not
    /// carried by this observability event.
    OutputHandleStored {
        provider_turn_id: String,
        call_id: String,
        handle_id: String,
        bytes: usize,
        lines: usize,
    },
    /// Terminal outcome of the post-change verification loop (issue #265),
    /// surfaced so Tier 3 renders an honest pass/fail/skipped line. Emitted at
    /// most once per turn, only when Iris changed files. Never claims pass on a
    /// failure and never suppresses the failing output.
    Verification(VerificationOutcome),
    /// The harness compacted persisted context at a safe turn boundary.
    /// Contains ids and token estimates, not the generated summary text.
    CompactionApplied {
        compaction_id: String,
        covered_from: String,
        covered_to: String,
        covered_messages: usize,
        original_tokens_estimate: u64,
        summary_tokens_estimate: u64,
        /// Provider-visible context estimate immediately after the atomic swap.
        context_tokens_after_apply: u64,
        budget: u64,
        /// 1-based compaction generation ordinal (ADR-0047): the Nth compaction
        /// in the session reports N. Instrumentation of compaction depth; does
        /// not affect range selection or summary content.
        generation: u64,
        /// Number of workspace-relative touched/read paths carried verbatim
        /// alongside the prose summary (ADR-0044). Additive instrumentation; 0
        /// when the covered range had no in-workspace tool targets.
        carried_paths: usize,
        /// Summary source. Provider-neutral and safe for persistence/metrics.
        origin: CompactionOrigin,
        /// Realized summarizer usage when the lane reported it.
        worker_usage: Option<ProviderUsage>,
    },
    /// Background compaction worker lifecycle (issue #472). Metadata only: the
    /// worker id, state, covered-count/token estimate, and a short status
    /// message. The generated summary text and covered transcript never ride
    /// this event; the parent emits [`CompactionApplied`] only after durable
    /// validation/persistence succeeds.
    CompactionLifecycle {
        job_id: String,
        state: CompactionLifecycleState,
        covered_messages: usize,
        original_tokens_estimate: u64,
        /// Summary source selected for this job.
        origin: CompactionOrigin,
        /// Realized usage once a worker has completed; `None` while running or
        /// when the provider does not report usage.
        worker_usage: Option<ProviderUsage>,
        /// Pressure tier that launched the job. `None` for manual compaction.
        trigger_tier: Option<ContextPressureTier>,
        message: Option<String>,
    },
    /// The harness flushed a batch of microcompaction folds at a safe turn
    /// boundary (ADR-0048, issue #400). Counts and estimates only, tagged with
    /// the trigger class that released the batch; never carries folded content.
    FoldApplied {
        /// Folds applied in this batch.
        folds: usize,
        /// Fold targets selected by semantic stale-read dedupe. A target also
        /// selected by clearing is counted in both reason totals but only once
        /// in `folds`.
        semantic_dedupe_folds: usize,
        /// Fold targets selected by local age/count clearing.
        tool_clearing_folds: usize,
        /// Estimated context tokens reclaimed (original bodies minus stubs).
        reclaimed_tokens_estimate: u64,
        /// Why the flush ran (design §4.4 trigger taxonomy).
        trigger: FoldTrigger,
    },
    AssistantText(String),
    AssistantTextDelta(String),
    AssistantTextEnd(String),
    /// One block of model reasoning ("thinking") surfaced for display. Reasoning
    /// arrives as whole blocks at turn completion (the provider stream does not
    /// expose incremental reasoning deltas here), so this is emitted once per
    /// block, not as a stream. Display-only: emitting it never changes what is
    /// stored (the reasoning row is still persisted, ADR-0016) or sent to the
    /// provider. A `redacted` block carries no text -- the provider withheld it,
    /// so the original reasoning text is never reconstructed or rendered; the
    /// front-end shows only that redacted reasoning occurred.
    AssistantReasoning {
        text: String,
        redacted: bool,
    },
    /// One incremental chunk of the model's reasoning text, streamed while
    /// the provider is still thinking (before any assistant text). Display-only,
    /// exactly like [`AssistantReasoning`]: emitting it never changes storage or
    /// what is sent to the provider, and the persisted reasoning row is still
    /// written once at completion. Encrypted/redacted reasoning is never
    /// reconstructed from these deltas (ADR-0016). When a turn streams these,
    /// the terminal [`AssistantReasoning`] display event for the same
    /// (non-redacted) block is suppressed so the finished thinking block is not
    /// shown twice.
    AssistantReasoningDelta(String),
    /// A boundary between two reasoning-summary parts (a blank line in the live
    /// thinking trace). Display-only; carries no text.
    AssistantReasoningSectionBreak,
    /// One incremental chunk of raw model reasoning. Display-only and explicitly
    /// separate from [`AssistantReasoningDelta`] so summary-vs-raw provenance is
    /// never lost while streaming. It never changes storage or provider replay.
    AssistantRawReasoningDelta(String),
    ToolProposed(ToolCall),
    /// A tool is about to execute (emitted once per call, immediately before the
    /// run, on both the exclusive and parallel paths). Lets a front-end open a
    /// live progress cell before any output arrives. Display-only.
    ToolStarted(ToolCall),
    /// A gated tool was auto-approved by the session allow-policy or the
    /// persistent project policy (ADR-0027). Emitted by Nexus, never inferred
    /// by a front-end, so the policy stays Nexus-owned.
    ToolAutoApproved(ToolCall),
    /// A gated tool was auto-approved because the session runs in
    /// `--dangerously-skip-permissions` mode (ADR-0049): the approval gate is
    /// bypassed for EVERY gated call, including calls a safety floor
    /// (destructive/dirty) would normally stop. A distinct, greppable audit
    /// event so this bypass is never silent and never confused with an
    /// ordinary policy auto-approval. Emitted by Nexus; the mode is explicit and
    /// never project/trust/env driven.
    ToolAutoApprovedDangerous(ToolCall),
    DiffPreview {
        call: ToolCall,
        diff: String,
    },
    ToolDenied(ToolCall),
    ToolResult {
        call: ToolCall,
        content: String,
        /// Process exit code for a shell-like tool (`Some(0)`/`None` is success,
        /// non-zero is failure). `None` for tools that have no exit status.
        exit_code: Option<i32>,
        /// Wall-clock execution time, when the tool reports it.
        duration: Option<Duration>,
    },
    /// One incremental fragment of a *freeform/custom* tool call's input, streamed
    /// while the model is still constructing the call (ADR-0039). Carries the
    /// streaming correlation id so a front-end could attach a live preview to the
    /// right call. Display-only and provably inert: the fragment is NEVER pushed
    /// to `self.messages`, never accumulated into `partial`/assistant text, and
    /// never merged into `AssistantTurn.tool_calls`. Approval and execution
    /// consume only the completed, validated `ToolCall` assembled at turn
    /// completion, so tampering with or dropping these deltas cannot change what
    /// runs. JSON-argument (`function`) tools do not emit this -- their arguments
    /// stay buffered until completion. No provider surfaces it in Iris today (no
    /// freeform tool is declared); the live preview UI is deferred until a
    /// freeform tool (`apply_patch`, V4A) exists to render.
    ToolInputDelta {
        call_id: String,
        delta: String,
    },
    /// A display-only chunk of a running tool's live output (issue #90 sub-item
    /// 1). Carries the originating call id so a front-end attaches it to the
    /// right live cell. NEVER pushed to `self.messages`: the full output reaches
    /// provider context once, via the tool's final `ToolResult`.
    ToolOutputDelta {
        call_id: String,
        chunk: String,
    },
    ToolError {
        call: ToolCall,
        message: String,
    },
    ToolCancelled(ToolCall),
    /// A user message the loop injected mid-run: a steering or follow-up message
    /// the host queued while the turn was running. Emitted so the front-end
    /// renders the user row in transcript order. The initial prompt is NOT
    /// emitted (the front-end commits that itself); only injected messages are.
    /// The message is also pushed into provider context.
    UserMessage(String),
    Notice(String),
    /// Dirty-tree state observed at the first mutating tool call of a task
    /// (issue #262). Carries a one-line human summary (dirty/untracked counts,
    /// or a degrade notice for a non-git / jj workspace); Tier 3 renders it so
    /// the user sees the pre-existing state before any file is touched.
    DirtyBaseline(String),
    /// A mutating call modified a protected (pre-existing dirty/untracked) file
    /// that was not approved for change (issue #262). The call is failed and, if
    /// `restored`, the file's pre-call contents were recovered from snapshot.
    /// Carries workspace-relative paths only, never file contents.
    MutationViolation {
        call: ToolCall,
        paths: Vec<String>,
        restored: bool,
    },
    TurnComplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionLifecycleState {
    Running,
    Ready,
    Applied,
    Discarded,
    Failed,
    Cancelled,
}

/// Provider-neutral source of an accepted compaction summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CompactionOrigin {
    Excerpts,
    Provider,
    Subagent,
    ProviderNative,
}

impl CompactionOrigin {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Excerpts => "excerpts",
            Self::Provider => "provider",
            Self::Subagent => "subagent",
            Self::ProviderNative => "providerNative",
        }
    }

    /// Human-facing route name for PROSE surfaces (apply notices, the transcript
    /// line). Identical to [`as_str`](Self::as_str) except `ProviderNative`
    /// reads `provider-native` instead of the camelCase `providerNative` that
    /// the machine-facing `/compaction` inspector and session log keep verbatim.
    pub(crate) const fn display_label(self) -> &'static str {
        match self {
            Self::ProviderNative => "provider-native",
            _ => self.as_str(),
        }
    }
}

impl CompactionLifecycleState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Applied => "applied",
            Self::Discarded => "discarded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolEventState {
    Proposed,
    ApprovalRequested,
    Approved,
    Denied,
    Started,
    Succeeded,
    Errored,
    Cancelled,
}

/// A provider-neutral streamed model event. The async [`ChatProvider`] yields a
/// sequence of these instead of one blocking whole-turn result, so the loop can
/// race each read against cancellation. Mirrors Codex's `ResponseEvent`
/// (`core/src/client.rs`): incremental text deltas, then one terminal
/// `Completed` carrying the assembled turn (text + tool calls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderEvent {
    /// Provider stream made progress that carries no user-visible text yet
    /// (reasoning/tool-call input deltas, pings, or other buffered SSE frames).
    /// The loop ignores it for transcript purposes, but it keeps transport idle
    /// detection from treating a live non-text stream as stalled.
    Activity,
    /// Incremental assistant text.
    TextDelta(String),
    /// Incremental reasoning-summary text (never raw chain-of-thought or
    /// encrypted/redacted content). Emitted by providers that surface live
    /// reasoning summaries before the answer; forwarded display-only.
    ReasoningDelta(String),
    /// Incremental raw reasoning text. Kept distinct from `ReasoningDelta` so
    /// provider/runtime/UI contracts never reclassify raw content as summary.
    RawReasoningDelta(String),
    /// A boundary between two reasoning-summary parts (blank line in the trace).
    ReasoningSectionBreak,
    /// One incremental fragment of a *freeform/custom* tool call's input
    /// (ADR-0039). Forwarded display-only and provably inert: never accumulated
    /// into the assembled turn's text or tool calls. Only freeform-tool adapters
    /// (currently the OpenAI Responses adapter, for `custom_tool_call` input)
    /// emit it; JSON-argument tool deltas stay buffered until completion.
    ToolInputDelta { call_id: String, delta: String },
    /// Terminal event: the fully assembled assistant turn.
    Completed(AssistantTurn),
}

/// Provider-native compaction support for the current selection and planned
/// input. Mimir decides this from adapter/model capability; upper tiers never
/// inspect provider ids or wire fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderCompactionCapability {
    None,
    OpaqueBlocks,
}

/// Successful provider-native reduction. `provider_blocks` are opaque adapter
/// envelopes: Nexus and Wayland persist and replay them without interpretation.
/// `summary` is always self-sufficient so another adapter can ignore the blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderCompactionOutput {
    pub(crate) summary: String,
    pub(crate) provider_blocks: Vec<Value>,
    pub(crate) usage: Option<ProviderUsage>,
}

/// A `!Send` boxed stream of provider events tied to the borrow of the provider
/// and its inputs. Boxed (not `impl Stream`) so the loop code is uniform and the
/// real provider can back it with a channel fed by a blocking task.
pub(crate) type ProviderStream<'a> = Pin<Box<dyn Stream<Item = Result<ProviderEvent>> + 'a>>;
pub(crate) type ProviderCompactionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderCompactionOutput>> + 'a>>;

/// Structured-output compaction-summary support for the current
/// provider/model/auth combination (issue #475, ADR-0061). Mimir decides this
/// from adapter capability; upper tiers never inspect provider ids or wire
/// fields -- mirrors [`ProviderCompactionCapability`], but for the portable
/// `wayland::compaction` summarizer route rather than the opt-in
/// provider-native `compact_context` axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum StructuredSummaryCapability {
    /// This provider/model/auth combination does not support the #475
    /// structured-output summary path; the caller keeps the existing
    /// full-transcript-replay summarizer instead. Default for every provider
    /// that does not opt in.
    #[default]
    None,
    /// Native structured output (`text.format`/`output_config.format`) should
    /// be attempted first.
    Native,
}

/// Which request shape a structured-output compaction-summary call sends
/// (issue #475).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StructuredSummaryMode {
    /// Provider-native structured output.
    Native,
    /// The forced single virtual tool (`emit_compaction_summary`) fallback,
    /// sent only after `Native` is rejected as deterministically unsupported.
    ForcedTool,
}

/// Why [`ChatProvider::run_structured_summary`] did not produce an
/// `AssistantTurn` for `wayland::structured_summary` to extract/validate.
#[derive(Debug)]
pub(crate) enum StructuredSummaryError {
    /// The active lane/model/auth kind deterministically rejected the request
    /// shape as unsupported (e.g. a 400 that is not a context-overflow body).
    /// Callers retry exactly once with [`StructuredSummaryMode::ForcedTool`].
    Unsupported,
    /// The turn was cancelled; callers must not fall back further (issue #475
    /// fallback order 4: skip compaction entirely on cancellation).
    Cancelled,
    /// Any other provider/transport failure; callers fall back to the
    /// existing deterministic excerpts.
    Other(Error),
}

pub(crate) type StructuredSummaryFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<AssistantTurn, StructuredSummaryError>> + 'a>>;

/// A `!Send` boxed tool-execution future, so `Box<dyn Tool>` stays object-safe
/// while `execute` is async.
pub(crate) type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput>> + 'a>>;

/// A `!Send` boxed approval future, so `&dyn ApprovalGate` stays object-safe
/// while `review` is async (and therefore raceable against cancellation).
pub(crate) type ApprovalFuture<'a> = Pin<Box<dyn Future<Output = Result<ApprovalDecision>> + 'a>>;
pub(crate) type InteractionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<InteractionOutcome>> + 'a>>;

/// A `!Send` governor future tied to the current loop boundary. The TUI runtime
/// is single-threaded; model-backed compaction work runs in its own worker and
/// only the bounded hard-tier wait may keep this future pending.
pub(crate) type ContextGovernorFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ContextDirective>> + 'a>>;
pub(crate) type ContextOverflowFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ContextOverflowRecovery>> + 'a>>;

/// Fire-and-forget observer for semantic events and provider-round-trip commit
/// boundaries. The default boundary hook is inert; Wayland uses it to persist
/// complete message groups without teaching Nexus about sessions or JSONL.
pub(crate) trait AgentObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()>;

    /// A complete provider round trip will be followed by another provider
    /// request. The snapshot ends on an answered assistant turn or a complete
    /// assistant-tool-call/tool-result group. Best-effort by contract: host
    /// persistence failures must not fail the user's turn.
    fn on_messages_committed(&self, _messages: &[Message]) {}
}

/// Persists oversized tool output outside provider context so the transcript
/// carries a compact handle instead of the full payload (issue #61). The Tier-2
/// Wayland harness implements it over local session storage; Nexus owns only
/// this contract and the threshold/compaction policy and never touches the
/// filesystem itself, mirroring [`ApprovalGate`]/[`ChatProvider`].
pub(crate) trait ToolOutputStore {
    /// Persist the full output text and return a stable handle id. The id must
    /// be stable for identical content so a resumed transcript keeps pointing at
    /// the same stored output (the harness impl is content-addressed).
    fn put(&self, content: &str) -> Result<String>;

    /// Retrieve a stored output by handle id, or `None` when no such handle
    /// exists (unknown or expired). The id comes from an (untrusted) transcript
    /// reference, so the impl validates it and returns `None` -- never an error
    /// or a filesystem escape -- for a malformed id. This is the retrieval half
    /// of the offload contract the model-facing `read_output` tool calls through
    /// [`ToolEnv::output_store`] (issue #205); keeping it on the contract, not
    /// the concrete store, holds the tier boundary that `put` already draws.
    fn get(&self, id: &str) -> Result<Option<String>>;
}

/// Read-only seam the `recall` tool uses to resolve a STANDALONE entry-id span
/// (ADR-0046 / issue #373): the original turns of a durable `[from, to]` id
/// range, read straight from THIS session's transcript rather than through a
/// compaction handle. The Tier-2 Wayland harness implements it over its OWN
/// session log, so it is scoped to a single session by construction -- a span
/// can never address another session's data or escape the session boundary.
/// Nexus owns only this contract (mirroring [`ToolOutputStore`]) and never
/// touches the filesystem itself. `None` on [`ToolEnv::session_span`] (tests,
/// in-memory sessions) means no standalone-span read path is available.
pub(crate) trait SessionSpanReader {
    /// Return the original turns whose durable entry id falls in the inclusive
    /// numeric `[from, to]` range, in transcript order, each paired with its
    /// durable id. The bounds are already parsed/validated by the caller; an
    /// empty vec means the span selected nothing (the caller turns that into a
    /// tool error for an explicit span). Errors only on a transcript read
    /// failure -- never a panic or a path escape.
    fn recall_span(&self, from: u64, to: u64) -> Result<Vec<(Option<String>, Message)>>;

    /// Return the original assistant tool-call and tool-result messages whose
    /// persisted `toolCallId` equals `tool_call_id`, in transcript order. An
    /// empty vec means this session contains no such call id.
    fn recall_tool_call(&self, tool_call_id: &str) -> Result<Vec<(Option<String>, Message)>>;
}

/// Display-only live-output sink for a running tool (issue #90 sub-item 1). A
/// long-running tool (today only `bash`) forwards each output chunk here as it
/// is produced; Nexus wraps it per-call and re-emits an
/// [`AgentEvent::ToolOutputDelta`] so the front-end can stream the command's
/// output live. `None` keeps a tool non-streaming (the parallel/exploration
/// path and tests inject `None`). Chunks are display-only and NEVER enter
/// provider context. Mirrors the optional [`ToolOutputStore`] seam.
pub(crate) trait ToolOutputSink {
    /// Forward one freshly produced output chunk (lossy-UTF-8 decoded) to the
    /// front-end. Best-effort: a delivery failure never aborts the tool.
    fn emit_chunk(&self, chunk: &str);
}

/// Source of mid-run user messages the host queued while a turn was running.
/// Provider- and UI-neutral, like [`ApprovalGate`]: Nexus owns the contract and
/// the injection points; the Tier-3 app backs it with the user's typed queue and
/// owns the drain policy (how many queued messages each poll yields). Mirrors
/// pi's `getSteeringMessages` / `getFollowUpMessages` loop hooks
/// (`packages/agent/src/types.ts`).
///
/// - **Steering** messages are injected before the next provider request (after
///   the current round's tool calls finish), so they redirect work in flight.
/// - **Follow-up** messages are injected only when the agent would otherwise
///   stop (no tool calls and no steering), continuing the run with another turn.
///
/// Both drains are synchronous and must not block: the in-memory queue is polled
/// from inside the loop between awaits. Each returns an empty vec when nothing is
/// queued.
pub(crate) trait SteeringSource {
    /// Drain the steering messages to inject before the next provider request.
    fn take_steering(&self) -> Vec<String>;
    /// Drain the follow-up messages to inject after the agent would stop.
    fn take_follow_up(&self) -> Vec<String>;
}

/// Most recent provider-reported usage plus the message prefix represented by
/// that report. The governor adds local estimates only after this prefix.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProviderUsageAnchor<'a> {
    pub(crate) usage: &'a ProviderUsage,
    pub(crate) message_count: usize,
}

/// Provider-neutral state at a safe between-round-trips boundary. The message
/// slice ends after complete tool-call/result groups; queued steering has not
/// been injected yet.
pub(crate) struct BoundaryContext<'a> {
    pub(crate) messages: &'a [Message],
    pub(crate) last_usage: Option<ProviderUsageAnchor<'a>>,
    pub(crate) round_trip: usize,
    pub(crate) turn_continues: bool,
}

/// Whole-context decision returned by a [`ContextGovernor`]. Nexus owns only
/// the atomic replacement; the governor owns every policy and persistence
/// decision that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContextDirective {
    Proceed,
    Replace { messages: Vec<Message> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContextOverflowRecovery {
    Resend {
        messages: Vec<Message>,
        measured: u64,
        effective_window: u64,
    },
    Unrecoverable {
        measured: u64,
        effective_window: u64,
    },
}

#[derive(Debug, Default, Clone, Copy)]
struct ReactiveOverflowState {
    attempted: bool,
    measurement: Option<(u64, u64)>,
}

/// Host-supplied context policy consulted only when another provider request
/// will follow. Object-safe like [`ApprovalGate`]; Nexus knows no budgets,
/// sessions, summaries, or provider-specific compaction behavior.
pub(crate) trait ContextGovernor {
    fn at_boundary<'a>(&'a self, cx: BoundaryContext<'a>) -> ContextGovernorFuture<'a>;

    fn on_context_overflow<'a>(&'a self, _cx: BoundaryContext<'a>) -> ContextOverflowFuture<'a> {
        Box::pin(async {
            Ok(ContextOverflowRecovery::Unrecoverable {
                measured: 0,
                effective_window: 0,
            })
        })
    }
}

/// Per-turn host hooks that travel together through the loop. Bundling the
/// observer and optional governor keeps the loop call surface compact while
/// preserving two distinct contracts.
#[derive(Clone, Copy)]
pub(crate) struct TurnContextHooks<'a> {
    pub(crate) observer: &'a dyn AgentObserver,
    pub(crate) governor: Option<&'a dyn ContextGovernor>,
}

/// Structured review facts Nexus derives at the gate and threads to the
/// front-end (issue #262/ADR-0010, ADR-0028). Facts only, never UI copy: Nexus
/// owns the contract and computes `destructive`/`dirty_paths` at the call site;
/// Tier 3 turns them into the explanatory reason line at the decision point
/// (docs/ARCHITECTURE.md tier rules). Cheap to clone (a bool plus a short list
/// of already-computed display paths), so it crosses the seam by value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReviewContext {
    /// The call performs a destructive, data-losing operation (the ADR-0010
    /// floor fired): even an allowed tool re-prompts. Drives a danger-toned
    /// note in the reason line.
    pub(crate) destructive: bool,
    /// Workspace-relative display paths of pre-existing uncommitted changes the
    /// call would touch (ADR-0028 dirty-tree gate). Non-empty means the dirty
    /// gate fired, so "always" here means "all dirty files (this task)" and no
    /// per-project grant is offered.
    pub(crate) dirty_paths: Vec<String>,
}

/// Request/response approval gate. Async so the loop can race a pending approval
/// against cancellation (`tokio::select!`); the loop branches on the returned
/// decision to control execution. Mirrors pi's `beforeToolCall` config hook,
/// which the loop inspects via `{ block }` -- a seam distinct from the event
/// sink.
pub(crate) trait ApprovalGate {
    /// `allow_always` mirrors the tool's [`Tool::supports_allow_always`] so the
    /// front-end only offers an "always allow" choice the loop will honor (shell
    /// tools opt out, so their prompt is y/N only). `allow_project` is true when
    /// a persistent per-project grant (ADR-0027) is on offer -- never for a
    /// destructive call (ADR-0010 floor). `ctx` carries the structured review
    /// facts (destructive floor, dirty-tree paths) the front-end renders into
    /// its reason line -- facts, never copy.
    fn review<'a>(
        &'a self,
        call: &'a ToolCall,
        allow_always: bool,
        allow_project: bool,
        ctx: ReviewContext,
    ) -> ApprovalFuture<'a>;

    /// Request a response for a tool whose purpose is human interaction rather
    /// than authorization. The safe default rejects, so approval-only hosts can
    /// never accidentally bypass a required question.
    fn interact<'a>(&'a self, _call: &'a ToolCall) -> InteractionFuture<'a> {
        Box::pin(async { Ok(InteractionOutcome::Rejected { feedback: None }) })
    }
}

/// Dirty-tree safety seam (issue #262, ADR-0028). Consulted by the core loop
/// around every mutating tool call so a pre-existing uncommitted change is never
/// silently damaged. Nexus owns the *enforcement* (routing a protected path
/// through the approval gate, halting a violating call); the *git knowledge*
/// lives entirely behind this contract in the Tier-2 harness, so the core loop
/// carries no baseline, hashing, or `git` awareness.
///
/// Contract, mirroring [`ApprovalGate`]/[`ChatProvider`]: the loop calls these
/// hooks; the implementation answers using the task baseline it captured lazily
/// on the first mutating call. Every method is synchronous and cheap by design
/// (the protected set is a known, small set of files); the harness may run
/// heavier attribution work asynchronously behind its own barriers.
pub(crate) trait MutationGuard {
    /// Called before the first (and every) mutating tool call. On the first
    /// mutation of a task it lazily captures the baseline and returns a
    /// one-line summary to surface (dirty/untracked counts, or a degrade
    /// notice); later calls in the same task return `None`. Pure Q&A turns
    /// never reach this, so they take no snapshot.
    fn note_mutation(&self) -> Option<String>;

    /// Of `paths`, which are in the dirty baseline and not yet approved this
    /// task. A non-empty result routes the call through the approval gate even
    /// when a session/project allow layer would otherwise auto-run it.
    fn unapproved_protected(&self, paths: &[PathBuf]) -> Vec<PathBuf>;

    /// Record approval for `paths` this task. `all_dirty` escalates the grant to
    /// every dirty file in the baseline (the prompt's "all dirty files this
    /// task" option). Approvals expire when the task settles.
    fn approve(&self, paths: &[PathBuf], all_dirty: bool);

    /// Prepare for a mutating call and return a preflight halt when execution
    /// must not proceed. The guard snapshots protected paths and known targets
    /// before execution. A non-empty result means the tool was not executed.
    fn before_exec(&self, paths: &[PathBuf]) -> GuardViolation;

    /// Re-check the protected set after a mutating call. `approved` are the
    /// paths this call was allowed to change; `expected_after` is the SHA-256
    /// hex of the exact bytes the tool reported writing (`None` when the tool
    /// did not confirm a write -- e.g. it failed, was cancelled, or is not a
    /// content-reporting tool). An approved path is recorded to the ledger as
    /// Iris-authored only when its post-call bytes match `expected_after`; on
    /// any mismatch (a concurrent user edit, or a failed/partial write) the
    /// change is ambiguous and, per ADR-0028's TOCTOU rule, treated as a
    /// user-attributed violation. Any *other* protected file that changed is
    /// likewise a violation for the loop to halt and offer restore.
    fn after_exec(&self, approved: &[PathBuf], expected_after: Option<&str>) -> GuardViolation;

    /// Restore the given protected files from the pre-exec snapshot. Best-effort
    /// recovery invoked by the loop after a detected violation.
    fn restore(&self, paths: &[PathBuf]) -> Result<()>;
}

/// Outcome of a mutation-guard preflight or postflight check: what, if
/// anything, requires the call to stop (issues #262, #560).
#[derive(Debug, Default)]
pub(crate) struct GuardViolation {
    /// Protected files that changed out-of-band, restorable from the pre-exec
    /// snapshot via [`MutationGuard::restore`].
    pub(crate) paths: Vec<PathBuf>,
    /// A halt the guard cannot attribute to named files: the working copy
    /// changed underneath it (e.g. an external jj operation) or its own
    /// post-call re-check failed. The loop fails the call with this reason;
    /// there is nothing file-level to restore beyond `paths`.
    pub(crate) reason: Option<String>,
}

impl GuardViolation {
    /// No violation: the call's changes are all accounted for.
    pub(crate) fn clean() -> Self {
        Self::default()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.reason.is_none()
    }
}

/// Structured result of a successful tool call: the model-facing text plus
/// Internal metadata key a mutating tool sets to the SHA-256 hex of the exact
/// bytes it wrote to its target (ADR-0028 write confirmation). The dirty-tree
/// guard confirms an approved change against this before attributing it to Iris;
/// [`record_call`] strips it before serialization, so it never reaches provider
/// context (like `exitCode`/`durationMs`). Not a git concept -- a content hash.
pub(crate) const WRITE_CONFIRM_HASH_KEY: &str = "__iris_after_hash";

/// A tool error that carries optional machine-readable classification beside
/// its human-readable message (ADR-0040, extending ADR-0021). Tools opt in by
/// returning it through their existing `anyhow::Result`; Nexus downcasts it in
/// the tool-error wire arm and emits a compact `metadata` object next to the
/// unchanged `error` string. Tools with nothing structured to report keep plain
/// `bail!` and their wire output stays byte-identical.
///
/// Keep `class` short and `fields` compact (ADR-0036): the payload is model
/// context, so carry only classification a consumer can act on, never large or
/// sensitive detail.
#[derive(Debug, Clone)]
pub(crate) struct ClassifiedError {
    class: String,
    message: String,
    fields: serde_json::Map<String, Value>,
}

impl ClassifiedError {
    /// A classified error with a stable `class` token and its human-readable
    /// message. The message stays as informative as an unclassified `bail!`;
    /// the class is additive.
    pub(crate) fn new(class: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            message: message.into(),
            fields: serde_json::Map::new(),
        }
    }

    /// Attach one compact classification field, builder-style.
    pub(crate) fn with(mut self, key: &str, value: Value) -> Self {
        self.fields.insert(key.to_string(), value);
        self
    }

    /// The stable machine-readable class token (ADR-0040), e.g. `not-found`,
    /// `not-unique`, or `stale-file`. Read-only accessor used by the
    /// tokens-per-task edit result-class probe to assert outcome classes
    /// against the stable token instead of matching on error prose.
    #[cfg(test)]
    pub(crate) fn class(&self) -> &str {
        &self.class
    }

    /// The model-facing `metadata` object: `{ "class": ..., ...fields }`.
    fn to_metadata(&self) -> serde_json::Map<String, Value> {
        let mut obj = serde_json::Map::new();
        obj.insert("class".to_string(), Value::String(self.class.clone()));
        for (key, value) in &self.fields {
            obj.insert(key.clone(), value.clone());
        }
        obj
    }
}

impl std::fmt::Display for ClassifiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClassifiedError {}

/// optional structured metadata. Tier-1 result contract (the analogue of pi's
/// `AgentToolResult`); tools with nothing structured to report use
/// [`ToolOutput::text`] and the metadata is omitted from the wire.
#[derive(Debug)]
pub(crate) struct ToolOutput {
    pub(crate) content: String,
    pub(crate) metadata: serde_json::Map<String, Value>,
}

impl ToolOutput {
    /// A text-only result with no structured metadata.
    pub(crate) fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            metadata: serde_json::Map::new(),
        }
    }

    /// Attach one metadata field, builder-style.
    pub(crate) fn with(mut self, key: &str, value: Value) -> Self {
        self.metadata.insert(key.to_string(), value);
        self
    }

    /// Attach the workspace-relative form of `requested` as `metadata.target`
    /// for the ADR-0044 compaction carry, when it resolves strictly inside
    /// `root`. The carry derives its touched/read path set from these successful
    /// results, so a read/ls/write/edit success records the file it acted on
    /// here. A path that escapes the workspace (absolute or `..` traversal)
    /// yields `None` from the strict carry floor and no `target` is attached, so
    /// an out-of-workspace path is never carried.
    pub(crate) fn with_workspace_target(self, root: &Path, requested: &str) -> Self {
        match crate::tools::path::workspace_relative(root, requested) {
            Some(rel) if !rel.is_empty() => self.with("target", Value::String(rel)),
            _ => self,
        }
    }
}

/// Execution environment handed to a tool: the workspace root plus the shared
/// per-session tool state (observed files, bash sessions). The state is behind a
/// [`RefCell`] so the loop can hand a shared `&ToolEnv` to several
/// concurrency-safe tools at once (safe-parallel execution); each tool's body is
/// synchronous and never holds the borrow across an `.await`. Owned by the
/// Tier-2 Wayland harness and injected into each turn, mirroring how pi's
/// `AgentHarness` feeds its `ExecutionEnv` into the loop. `ToolState` is defined
/// in `crate::tools`.
pub(crate) struct ToolEnv<'a> {
    pub(crate) workspace: &'a Path,
    pub(crate) state: &'a RefCell<crate::tools::ToolState>,
    /// Optional out-of-context store for oversized tool outputs (issue #61).
    /// `None` keeps every output inline (no durable session storage available),
    /// preserving the original in-memory behavior. Harness-owned, injected here.
    pub(crate) output_store: Option<&'a dyn ToolOutputStore>,
    /// Optional read-only session-span seam for the `recall` tool's standalone
    /// entry-id span (ADR-0046 / issue #373). `None` (tests, in-memory session)
    /// disables the standalone-span path; the harness injects a reader over its
    /// OWN transcript, so a span stays scoped to this session. Harness-owned.
    pub(crate) session_span: Option<&'a dyn SessionSpanReader>,
    /// Optional live-output sink for streaming a running tool's output (issue
    /// #90 sub-item 1). `None` (the harness default) keeps the tool
    /// non-streaming; Nexus injects a per-call sink on the exclusive path so a
    /// streaming tool's chunks reach the front-end as display-only deltas.
    pub(crate) output_sink: Option<&'a dyn ToolOutputSink>,
    /// Optional dirty-tree safety guard (issue #262). `None` (tests, non-git or
    /// degraded harness) disables dirty gating entirely, preserving the original
    /// approval behavior. The Tier-2 harness injects it so the core loop can
    /// gate mutating calls without any git knowledge of its own.
    pub(crate) mutation_guard: Option<&'a dyn MutationGuard>,
}

/// A tool the agent can invoke. Mirrors pi-ai's `Tool`
/// (`name`/`description`/`parameters`) plus pi-agent's `AgentTool` (the tool
/// runs itself via `execute`). Nexus enforces the approval policy, but each tool
/// *classifies* itself, so the core loop never matches on tool names.
pub(crate) trait Tool {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the arguments, used to build provider tool declarations.
    fn parameters(&self) -> Value;
    /// Run the tool. Async + given a child [`CancellationToken`]: a long-running
    /// tool (e.g. shell) should observe the token and stop promptly. The loop
    /// also races this future against the token, so a tool that ignores it is
    /// still abandoned (with a synthetic cancelled result).
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        cancel: CancellationToken,
    ) -> ToolFuture<'a>;

    /// Whether this tool may run concurrently with other concurrency-safe tools
    /// in the same model turn. Default: exclusive. Only read-only tools whose
    /// behavior is unaffected by concurrent peers opt in; the loop never runs an
    /// exclusive tool alongside anything else.
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    /// Whether a call to this tool must be approved before it runs.
    fn requires_approval(&self) -> bool {
        false
    }
    /// Whether execution must pause for a human response. This is independent
    /// of permission policy and therefore cannot be bypassed by approval modes.
    fn requires_user_interaction(&self) -> bool {
        false
    }
    /// Whether this tool mutates the workspace, so the dirty-tree guard (issue
    /// #262) should capture a baseline on the first such call in a task and
    /// snapshot/verify the protected set around this call. Default: read-only.
    /// `edit`/`write`/`bash` opt in; the core loop never name-matches them.
    fn is_mutating(&self) -> bool {
        false
    }
    /// The concrete workspace paths this call will modify, when statically known
    /// from the arguments (e.g. `edit`/`write` target their `file_path`/`path`).
    /// Used by the dirty-tree guard to decide whether a call touches a
    /// pre-existing dirty file and must route through approval. Empty when the
    /// set is not statically knowable (e.g. an arbitrary `bash` command), which
    /// falls back to the snapshot/verify detection path instead.
    fn mutates_paths(&self, _args: &Value) -> Vec<PathBuf> {
        Vec::new()
    }
    /// Whether these arguments perform a destructive, data-losing operation that
    /// must be re-approved every time, even when the tool is "always allowed".
    fn is_destructive(&self, _args: &Value) -> bool {
        false
    }
    /// Whether an "always allow" decision may persist for this tool. Tools that
    /// authorize arbitrary later effects (e.g. shell) opt out, so the loop keeps
    /// prompting each call instead of name-matching `"bash"` in core.
    fn supports_allow_always(&self) -> bool {
        true
    }
    /// Whether this exact call is eligible for deterministic auto-approval under
    /// the `auto` preset (ADR-0032), ASSUMING no floor blocks it. This is a
    /// classification request, never an authorization (ADR-0014): Nexus consults
    /// it only when the session approval mode is `Auto`, and always applies the
    /// destructive and dirty-tree floors first. Default: not auto-eligible, so a
    /// tool must opt in. `edit`/`write` opt in for in-workspace targets; `bash`
    /// does not (its safe-auto path needs a proven sandbox preflight, deferred).
    fn auto_approvable(&self, _workspace: &Path, _args: &Value) -> bool {
        false
    }
    /// Optional pre-approval diff preview (Tier-3 presentation). `None` when the
    /// tool has no preview or the arguments are malformed.
    fn diff_preview(&self, _workspace: &Path, _args: &Value) -> Option<String> {
        None
    }
}

/// Provider/model capability snapshot the tool-surface planner reads to decide
/// which built-in tools are *model-visible*. Provider-neutral (Tier 1): each
/// provider reports its own via [`ChatProvider::capabilities`], and the planner
/// ([`Tools::plan_surface`]) maps it to the advertised tool set. The default
/// exposes the full built-in surface, so every provider that does not override
/// keeps today's exact behavior.
///
/// This narrows only what the model *sees*; execution lookup ([`Tools::by_name`])
/// is unaffected, so a hidden tool a resumed transcript still references stays
/// runnable. Conceptually mirrors Codex's split between a tool registry
/// (execution) and `model_visible_specs` (declarations) without porting its
/// planner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProviderCapabilities {
    /// The provider/model carries a native code-edit affordance (e.g. Codex
    /// `apply_patch`, V4A) that supersedes the generic `edit` tool, so `edit` is
    /// dropped from the model-visible surface. The native replacement tool is a
    /// later slice (ROADMAP #10 provider-specific tools); no provider sets this
    /// today, so the surface is unchanged.
    pub(crate) native_edit: bool,
}

impl ProviderCapabilities {
    /// Whether a tool with this name is advertised to the model under these
    /// capabilities. The single rule today: a provider with a native edit
    /// affordance hides the generic `edit` tool. Any other name is exposed
    /// (fail-open), so missing or partial capability data falls back to today's
    /// full surface.
    fn exposes(&self, tool_name: &str) -> bool {
        !(self.native_edit && tool_name == "edit")
    }
}

/// Injected collection the agent resolves tool calls against. A thin name lookup
/// over a `Vec<Box<dyn Tool>>` -- no identity keys, override, or dispatch-order
/// machinery (that is issue #18, out of scope). Mirrors pi's `context.tools`
/// resolved with `tools.find(t => t.name === toolCall.name)`.
///
/// The set distinguishes the *model-visible* surface ([`iter`](Self::iter), used
/// to build provider declarations) from the full *execution* registry
/// ([`by_name`](Self::by_name)). They are deliberately separate: the planner can
/// hide a tool from the model while keeping it runnable, the seam
/// provider-specific tool surfaces (ROADMAP #10) build on.
pub(crate) struct Tools {
    tools: Vec<Box<dyn Tool>>,
    /// Capabilities of the provider/model this set is advertised to.
    /// [`iter`](Self::iter) (model-visible declarations) filters by this;
    /// [`by_name`](Self::by_name) (execution) ignores it. Defaults to full
    /// exposure, so a freshly built set advertises every tool until a planner
    /// narrows it.
    caps: ProviderCapabilities,
}

impl Tools {
    pub(crate) fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self {
            tools,
            caps: ProviderCapabilities::default(),
        }
    }

    /// Resolve a call by exact tool name over the *full* registry. `None` for an
    /// unknown tool. Independent of the model-visible plan: a tool hidden by
    /// [`plan_surface`](Self::plan_surface) still resolves here, so execution,
    /// approval, and cancellation behavior never depends on visibility.
    pub(crate) fn by_name(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .map(|tool| &**tool)
            .find(|tool| tool.name() == name)
    }

    /// Iterate the *model-visible* tools in declaration order (for provider tool
    /// schemas). Reflects the planned surface, not the full registry.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools
            .iter()
            .map(|tool| &**tool)
            .filter(|tool| self.caps.exposes(tool.name()))
    }

    /// Build a registry that contains only read-only, ungated tools. This is
    /// used for read-only subagents so the model-visible declarations and the
    /// execution lookup are narrowed by the same contract: a hidden or resumed
    /// mutating call cannot run because it is absent from [`by_name`](Self::by_name),
    /// not merely omitted from [`iter`](Self::iter).
    pub(crate) fn into_read_only(self) -> Self {
        Self {
            tools: self
                .tools
                .into_iter()
                .filter(|tool| {
                    !tool.is_mutating()
                        && !tool.requires_approval()
                        && !tool.requires_user_interaction()
                })
                .collect(),
            caps: self.caps,
        }
    }

    /// Keep only tools named in the caller-supplied allowlist. Used after a
    /// capability filter, so an allowlist can narrow the registry but cannot
    /// reintroduce tools removed by policy.
    pub(crate) fn into_allowlist(self, names: &[String]) -> Self {
        let names: BTreeSet<&str> = names.iter().map(String::as_str).collect();
        Self {
            tools: self
                .tools
                .into_iter()
                .filter(|tool| names.contains(tool.name()))
                .collect(),
            caps: self.caps,
        }
    }

    /// Plan the model-visible surface for a provider/model with these
    /// capabilities. The registry (every tool) is untouched, so [`by_name`](Self::by_name)
    /// still resolves hidden tools for execution; only the advertised
    /// [`iter`](Self::iter) surface shrinks. This is the planner seam: it records
    /// the capabilities the surface is planned against, never branching inside
    /// tool execution.
    fn plan_surface(&mut self, caps: &ProviderCapabilities) {
        self.caps = *caps;
    }
}

pub(crate) trait ChatProvider {
    /// Begin a streamed model response. Providers translate their native wire
    /// format into a stream of Nexus-owned [`ProviderEvent`]s; setup errors
    /// (e.g. a bad URL) surface synchronously via the `Result`, stream errors
    /// arrive as `Err` items. `tools` is the injected set the provider
    /// advertises as callable declarations. `cancel` is the turn token: a
    /// provider that does blocking work off-thread should observe it so a
    /// cancelled turn stops issuing/retrying requests instead of running to
    /// completion in the background.
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>>;

    /// The provider/model capabilities the tool-surface planner reads to decide
    /// the model-visible tool set. Default: the full built-in surface (today's
    /// behavior). A provider overrides this only to vary which tools the model
    /// sees; tool execution is never affected.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Whether this selection can produce a portable text summary plus opaque
    /// replay block for the planned input size. The input estimate lets Mimir
    /// enforce provider-native trigger floors without leaking them upward.
    fn compaction_capability(&self, _input_tokens: u64) -> ProviderCompactionCapability {
        ProviderCompactionCapability::None
    }

    /// Run one provider-native compaction request. Called only after capability
    /// selection and off the parent loop; the default is a typed unsupported
    /// error so adapters opt in explicitly.
    fn compact_context<'a>(
        &'a self,
        _messages: &'a [Message],
        _instructions: &'a str,
        _cancel: &'a CancellationToken,
    ) -> ProviderCompactionFuture<'a> {
        Box::pin(async { bail!("provider-native compaction is not supported") })
    }

    /// Structured-output compaction-summary capability for this provider/model
    /// (issue #475, ADR-0061). Default: unsupported, so the compaction
    /// summarizer route (`wayland::compaction`) keeps its existing
    /// full-transcript-replay path unless a provider opts in explicitly.
    fn structured_summary_capability(&self) -> StructuredSummaryCapability {
        StructuredSummaryCapability::None
    }

    /// Send one structured-output compaction-summary request (native or
    /// forced-virtual-tool, per `mode`) and return the resulting
    /// `AssistantTurn` for `wayland::structured_summary` extraction. Default:
    /// a typed unsupported error so adapters opt in explicitly, mirroring
    /// [`ChatProvider::compact_context`].
    fn run_structured_summary<'a>(
        &'a self,
        _messages: &'a [Message],
        _mode: StructuredSummaryMode,
        _cancel: &'a CancellationToken,
    ) -> StructuredSummaryFuture<'a> {
        Box::pin(async {
            Err(StructuredSummaryError::Other(anyhow!(
                "structured-output compaction summaries are not supported by this provider"
            )))
        })
    }
}

/// Forward the contract through a boxed provider so the front-end can select
/// one of several concrete providers at runtime (`Box<dyn ChatProvider>`)
/// without making every downstream type generic over the choice.
impl ChatProvider for Box<dyn ChatProvider> {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        (**self).respond_stream(messages, tools, cancel)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        (**self).capabilities()
    }

    fn compaction_capability(&self, input_tokens: u64) -> ProviderCompactionCapability {
        (**self).compaction_capability(input_tokens)
    }

    fn compact_context<'a>(
        &'a self,
        messages: &'a [Message],
        instructions: &'a str,
        cancel: &'a CancellationToken,
    ) -> ProviderCompactionFuture<'a> {
        (**self).compact_context(messages, instructions, cancel)
    }

    fn structured_summary_capability(&self) -> StructuredSummaryCapability {
        (**self).structured_summary_capability()
    }

    fn run_structured_summary<'a>(
        &'a self,
        messages: &'a [Message],
        mode: StructuredSummaryMode,
        cancel: &'a CancellationToken,
    ) -> StructuredSummaryFuture<'a> {
        (**self).run_structured_summary(messages, mode, cancel)
    }
}

pub(crate) struct Agent<P> {
    pub(crate) provider: P,
    messages: Vec<Message>,
    // Injected tool set, constructed at Tier 3 and resolved by name in the loop.
    // Core names no concrete tool; it only holds the `Tool` contract.
    tools: Tools,
    // Session approval policy: tool names the user chose to "always" allow.
    // Owned and enforced here in Nexus, not in the UI, so a front-end can never
    // silently widen what runs without approval. Granularity is per tool name.
    // ponytail: per-tool-name always-allow. The mutating tools (`bash`, `write`,
    // `edit`) opt out (`supports_allow_always() == false`), so an "always" on
    // them never sticks and every call re-prompts -- a blanket allow would
    // authorize arbitrary later effects. Persistent per-project grants (the
    // finer per-tool/per-exact-command layer) live in `project_policy` below
    // (ADR-0027, issue #209).
    session_allowed: HashSet<String>,
    // Persistent per-project (per-cwd) permission policy: the "project" layer
    // between the session allow-set and the global prompt default (ADR-0027).
    // Loaded by the host from the HOME-owned store and kept in sync as grants
    // are made; enforced here, never in a front-end. Survives reset_session
    // (it is keyed to the cwd, not the conversation).
    project_policy: ProjectPolicy,
    // Persistence seam for new project grants. `None` (e.g. headless print)
    // keeps a grant in-memory for the process lifetime only.
    policy_sink: Option<Box<dyn ProjectPolicySink>>,
    // Operator-selected approval preset (ADR-0032). Installed by the host at
    // construction and changed at inter-turn boundaries via `set_approval_mode`.
    // Nexus owns the enforcement: the mode only decides prompt-vs-auto-vs-deny
    // for a call not already blocked by a floor or covered by an explicit grant.
    approval_mode: ApprovalMode,
    // `--dangerously-skip-permissions` / dangerous permission mode (ADR-0049):
    // when true, EVERY gated tool call is auto-approved, including calls a
    // safety floor (destructive/dirty) would normally stop. Set only by the host
    // from operator-controlled runtime state (CLI flag, global defaultApproval,
    // or the session transcript being resumed); project config, trust stores,
    // env, and repo paths cannot enable it.
    // No grant is ever persisted while it is on, and turning it off restores the
    // normal approval-mode path.
    skip_permissions: bool,
    // Provider/model round-trip id sequence. Nexus owns these ids because it
    // owns the provider loop; Wayland may persist them, but never mints them.
    next_provider_turn_seq: u32,
    // Optional graceful soft cap on tool round-trips per turn. `None` (default)
    // = unbounded: the loop runs while the model emits tool calls, matching
    // pi-mono. When `Some(n)`, the loop ends the turn with a Notice after `n`
    // round-trips instead of a fatal error. Cancellation remains the always-on
    // runaway guard regardless. Supplied by the host from `Settings`.
    max_tool_roundtrips: Option<usize>,
    // Whether a mutating tool call (edit/write/bash) SUCCEEDED during the turn
    // currently running or just completed. Reset at the start of every
    // `submit_turn` and set when a mutating call's final outcome is `Ok`, so the
    // harness can tell an end-of-turn "Iris changed files" from a pure Q&A turn
    // (issue #265 post-change verification trigger). Independent of git state,
    // so it is correct in non-git/degraded workspaces too. Read only right after
    // a `submit_turn` returns; a directly-run verification command
    // ([`run_verification_command`]) may also set it, but that value is always
    // overwritten by the next `submit_turn` reset before the harness reads it.
    mutated_this_turn: bool,
    // Last authoritative provider usage and the transcript length it covered.
    // Wayland adds estimates only for messages appended after this anchor.
    last_provider_usage: Option<(ProviderUsage, usize)>,
}

/// Result of consuming one provider stream to its terminal event (or to a
/// cancellation). Owned so the borrow of `self.messages`/`self.tools` taken by
/// the stream is released before the loop mutates the transcript.
enum StreamResult {
    Completed {
        // Boxed: `AssistantTurn` is large, so an unboxed variant makes
        // `StreamResult` lopsided (clippy::large_enum_variant).
        turn: Box<AssistantTurn>,
        saw_delta: bool,
        /// Whether any reasoning delta was forwarded for display during
        /// this stream. When true, the terminal reasoning display event for the
        /// (non-redacted) summary is suppressed so the live thinking block the
        /// front-end already showed is not duplicated.
        saw_reasoning_delta: bool,
        /// Instant of the FIRST non-empty streamed output delta (text,
        /// reasoning, or tool-call argument), or `None` when the provider
        /// completed without streaming a non-empty delta. Consumed by the caller
        /// to derive time-to-first-output relative to the request-send instant.
        first_output: Option<Instant>,
    },
    Cancelled {
        partial: String,
        saw_delta: bool,
    },
}

/// Whether the tool phase wants another model round-trip or ended the turn
/// itself (a cancellation already emitted `TurnComplete`).
enum ToolsPhase {
    Continue,
    Ended,
}

/// Internal per-call execution outcome, mapped to a transcript message + event
/// by [`record_call`].
enum ToolOutcome {
    Ok(ToolOutput),
    Err(anyhow::Error),
    Cancelled,
    Denied,
    DeniedWithFeedback(String),
}

fn guard_violation_outcome(
    guard: &dyn MutationGuard,
    violation: GuardViolation,
    call: &ToolCall,
    workspace: &Path,
    obs: &dyn AgentObserver,
    executed: bool,
) -> Result<ToolOutcome> {
    let restored =
        executed && !violation.paths.is_empty() && guard.restore(&violation.paths).is_ok();
    let paths: Vec<String> = violation
        .paths
        .iter()
        .map(|path| {
            path.strip_prefix(workspace)
                .unwrap_or(path)
                .display()
                .to_string()
        })
        .collect();
    let what = match &violation.reason {
        Some(reason) => reason.clone(),
        None => format!(
            "modified protected uncommitted file(s): {}",
            paths.join(", ")
        ),
    };
    let status = if executed { "halted" } else { "not executed" };
    let recovery = if !executed || violation.paths.is_empty() {
        ""
    } else if restored {
        "; restored from snapshot"
    } else {
        "; snapshot restore failed"
    };
    if executed && !violation.paths.is_empty() {
        obs.on_event(AgentEvent::MutationViolation {
            call: call.clone(),
            paths,
            restored,
        })?;
    } else {
        obs.on_event(AgentEvent::Notice(format!("{status}: {what}")))?;
    }
    Ok(ToolOutcome::Err(anyhow::anyhow!(
        "{status}: {what}{recovery}"
    )))
}

/// Classified result of one gated verification-command run (issue #265),
/// returned by [`Agent::run_verification_command`] so the harness's loop can
/// decide pass / retry / report-skipped without re-parsing tool metadata.
pub(crate) enum VerifyRun {
    /// The command exited 0.
    Passed,
    /// The command exited non-zero (or reported no status / errored). `output`
    /// is the command's rendered output, fed back to the model on retry and
    /// surfaced (truncated) in the final failure report.
    Failed {
        output: String,
        exit_code: Option<i32>,
    },
    /// The user denied the verification command's approval prompt.
    Denied,
    /// The turn was cancelled while the verification command was gated/running.
    Cancelled,
}

impl<P: ChatProvider> Agent<P> {
    /// A bare, in-memory agent: it owns the provider, conversation, injected
    /// tools, and approval policy, but no filesystem or persistence. Mirrors
    /// pi's bare `Agent`; the Tier-2 Wayland harness wraps it with the execution
    /// env and session store.
    pub(crate) fn new(provider: P, mut tools: Tools) -> Self {
        // Plan the model-visible surface once, from the provider's declared
        // capabilities. Default capabilities expose the full built-in set, so
        // every provider keeps today's surface unless it opts to vary it.
        tools.plan_surface(&provider.capabilities());
        Self {
            provider,
            messages: Vec::new(),
            tools,
            session_allowed: HashSet::new(),
            project_policy: ProjectPolicy::default(),
            policy_sink: None,
            approval_mode: ApprovalMode::default(),
            skip_permissions: false,
            next_provider_turn_seq: 0,
            max_tool_roundtrips: None,
            mutated_this_turn: false,
            last_provider_usage: None,
        }
    }

    /// Install the persistent per-project permission policy and its persistence
    /// sink (ADR-0027). The host loads the policy from the HOME-owned store for
    /// the session's cwd; the sink writes new grants back to it.
    pub(crate) fn with_project_policy(
        mut self,
        policy: ProjectPolicy,
        sink: Option<Box<dyn ProjectPolicySink>>,
    ) -> Self {
        self.project_policy = policy;
        self.policy_sink = sink;
        self
    }

    /// Replace the in-memory project policy (the `/trust` editor applies
    /// grant/revoke edits at the safe inter-turn boundary).
    pub(crate) fn set_project_policy(&mut self, policy: ProjectPolicy) {
        self.project_policy = policy;
    }

    /// Change the approval preset (ADR-0032). Called by the host at a safe
    /// inter-turn boundary (the `/approval` control); enforcement stays here.
    pub(crate) fn set_approval_mode(&mut self, mode: ApprovalMode) {
        self.approval_mode = mode;
    }

    /// The active approval preset, for the host to render the status label.
    pub(crate) fn approval_mode(&self) -> ApprovalMode {
        self.approval_mode
    }

    /// Enable dangerous skip-permissions (ADR-0049) for this runtime state.
    /// When on, every gated call is auto-approved, floors included, and no grant
    /// is ever persisted.
    pub(crate) fn with_skip_permissions(mut self, skip: bool) -> Self {
        self.skip_permissions = skip;
        self
    }

    /// Change dangerous skip-permissions at a safe inter-turn boundary. The host
    /// controls activation and persistence; Nexus owns the enforcement.
    pub(crate) fn set_skip_permissions(&mut self, skip: bool) {
        self.skip_permissions = skip;
    }

    /// Whether this session runs in `--dangerously-skip-permissions` mode.
    pub(crate) fn skip_permissions(&self) -> bool {
        self.skip_permissions
    }

    /// Install an optional graceful soft cap on tool round-trips per turn.
    /// `None` keeps the default unbounded loop; `Some(n)` ends the turn with a
    /// Notice after `n` round-trips. The host threads the configured value from
    /// `Settings` exactly like the context token budget.
    pub(crate) fn with_max_tool_roundtrips(mut self, cap: Option<usize>) -> Self {
        self.max_tool_roundtrips = cap;
        self
    }

    /// A bare agent seeded with a prior conversation, for resuming a session.
    /// The reconstructed `messages` become the provider-visible context for the
    /// next turn; the approval policy starts fresh (allow-always is per-process,
    /// not persisted). Mirrors pi's harness loading session entries and
    /// rebuilding context before continuing the conversation.
    pub(crate) fn resumed(provider: P, mut tools: Tools, mut messages: Vec<Message>) -> Self {
        repair_dangling_tool_call(&mut messages);
        let next_provider_turn_seq = next_provider_turn_seq(&messages);
        tools.plan_surface(&provider.capabilities());
        Self {
            provider,
            messages,
            tools,
            session_allowed: HashSet::new(),
            project_policy: ProjectPolicy::default(),
            policy_sink: None,
            approval_mode: ApprovalMode::default(),
            skip_permissions: false,
            next_provider_turn_seq,
            max_tool_roundtrips: None,
            mutated_this_turn: false,
            last_provider_usage: None,
        }
    }

    /// Read access to the in-memory transcript so the harness can persist it
    /// without the core loop owning a session store.
    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub(crate) fn last_provider_usage_anchor(&self) -> Option<(u64, usize)> {
        self.last_provider_usage
            .as_ref()
            .map(|(usage, count)| (usage.total_tokens, *count))
    }

    /// Whether a mutating tool call succeeded during the turn just run (issue
    /// #265). The harness reads this right after [`submit_turn`] to decide
    /// whether to run post-change verification; a pure Q&A turn returns `false`.
    pub(crate) fn mutated_this_turn(&self) -> bool {
        self.mutated_this_turn
    }

    /// Read access to the injected tool set so the harness can issue auxiliary
    /// provider requests (compaction summaries) that advertise the same
    /// model-visible declarations as a normal turn, keeping the provider's
    /// cached prompt prefix (tools + system + history) intact.
    pub(crate) fn tools(&self) -> &Tools {
        &self.tools
    }

    /// Replace the in-memory provider-visible context. The Tier-2 harness uses
    /// this to install a compacted context (summary + retained tail) before the
    /// next turn; the bare agent stays oblivious to compaction policy and
    /// persistence, just as [`resumed`](Self::resumed) seeds context on resume.
    pub(crate) fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        self.last_provider_usage = None;
    }

    /// Reset the in-memory conversation to a different session's context at a
    /// safe turn boundary (the in-session `/resume` and `/new` swap). Mirrors
    /// [`resumed`](Self::resumed)'s message handling -- repair a crash-truncated
    /// trailing tool call and recompute the provider-turn sequence -- and clears
    /// the session allow-always policy so a new/other session never inherits the
    /// prior one's approvals. The provider is swapped separately via
    /// [`replace_provider`](Self::replace_provider); an empty `messages` starts a
    /// fresh transcript.
    pub(crate) fn reset_session(&mut self, mut messages: Vec<Message>) {
        repair_dangling_tool_call(&mut messages);
        self.next_provider_turn_seq = next_provider_turn_seq(&messages);
        self.messages = messages;
        self.last_provider_usage = None;
        self.session_allowed.clear();
    }

    /// Swap the active provider at a safe turn boundary and re-plan the
    /// model-visible tool surface from the new provider's capabilities. The
    /// Tier-3 app rebuilds a provider on a `/model` `/reasoning` switch and
    /// installs it here; the in-memory conversation and approval policy are
    /// untouched, so the next turn runs against the new provider with the same
    /// context. Mirrors [`new`](Self::new)/[`resumed`](Self::resumed) planning
    /// the surface once from the provider's capabilities.
    pub(crate) fn replace_provider(&mut self, provider: P) {
        self.provider = provider;
        self.tools.plan_surface(&self.provider.capabilities());
        self.last_provider_usage = None;
    }

    pub(crate) async fn submit_turn(
        &mut self,
        prompt: &str,
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
        steer: Option<&dyn SteeringSource>,
    ) -> Result<()> {
        self.submit_turn_with_context_and_governor(
            TurnInput::new(prompt),
            TurnContextHooks {
                observer: obs,
                governor: None,
            },
            gate,
            env,
            token,
            steer,
        )
        .await
    }

    /// Submit one turn with a host-supplied between-round-trips context policy.
    /// Normal bare-agent callers use [`submit_turn`](Self::submit_turn), whose
    /// governor is inert (`None`).
    pub(crate) async fn submit_turn_with_governor(
        &mut self,
        prompt: &str,
        hooks: TurnContextHooks<'_>,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
        steer: Option<&dyn SteeringSource>,
    ) -> Result<()> {
        self.submit_turn_with_context_and_governor(
            TurnInput::new(prompt),
            hooks,
            gate,
            env,
            token,
            steer,
        )
        .await
    }

    /// Submit one visible prompt with hidden provider-context messages and an
    /// optional governor. Wayland uses this entry point so skill injections and
    /// compaction governance share one loop; Nexus interprets neither policy.
    pub(crate) async fn submit_turn_with_context_and_governor(
        &mut self,
        input: TurnInput<'_>,
        hooks: TurnContextHooks<'_>,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
        steer: Option<&dyn SteeringSource>,
    ) -> Result<()> {
        // Reset the per-turn mutation signal so the harness's post-change
        // verification trigger (issue #265) reflects only this turn: a pure Q&A
        // turn stays `false` and runs no verification. A retry turn resets it too,
        // so "did the model make further changes" is measured fresh each attempt.
        self.mutated_this_turn = false;
        // Start of the contextual-message + visible-prompt run. A cancellation
        // before any provider answer truncates the entire unanswered input.
        let unanswered_start = self.messages.len();
        self.messages.extend(input.context);
        self.messages.push(Message::user(input.prompt));
        // The bare agent does no persistence. It announces complete round-trip
        // boundaries to the host; the harness also diffs `messages()` after the
        // turn as the final/error backstop.
        self.complete_turn(unanswered_start, hooks, gate, env, token, steer)
            .await
    }

    /// Run one post-change verification command (issue #265) as a NORMAL gated
    /// shell execution: a `bash` tool call routed through the exact same
    /// approval gate, dirty-tree guard (#262), and cancellation path as any
    /// model-issued shell call. No approval bypass and no persistent
    /// allow-always -- `bash` opts out (ADR-0010), so it re-prompts every run.
    ///
    /// The command's outcome is emitted as a display-only tool result (so Tier 3
    /// renders the run and its output) but is NOT recorded into provider context
    /// here: the harness feeds a failure back to the model as an explicit user
    /// message instead of fabricating an assistant tool call. Returns the
    /// classified [`VerifyRun`] so the harness can decide pass/fail/retry.
    pub(crate) async fn run_verification_command(
        &mut self,
        command: &str,
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
    ) -> Result<VerifyRun> {
        let provider_turn_id = self.next_provider_turn_id();
        let call = ToolCall {
            id: format!("verify_{provider_turn_id}"),
            name: "bash".to_string(),
            arguments: json!({ "command": command }),
            thought_signature: None,
        };
        let outcome = self
            .run_gated_single(&call, obs, gate, env, token, &provider_turn_id)
            .await?;
        // Classify before `record_call` consumes the outcome. Pass = exit 0;
        // any non-zero/absent status or tool error is a failure carrying the
        // command output (the guard-halt error text, when that is the failure).
        let run = match &outcome {
            ToolOutcome::Ok(output) => {
                let exit_code = output
                    .metadata
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .map(|code| code as i32);
                if exit_code == Some(0) {
                    VerifyRun::Passed
                } else {
                    VerifyRun::Failed {
                        output: output.content.clone(),
                        exit_code,
                    }
                }
            }
            ToolOutcome::Err(error) => VerifyRun::Failed {
                output: format!("{error:#}"),
                exit_code: None,
            },
            ToolOutcome::Denied | ToolOutcome::DeniedWithFeedback(_) => VerifyRun::Denied,
            ToolOutcome::Cancelled => VerifyRun::Cancelled,
        };
        // Emit the display-only tool result (proposed/started already fired in
        // `run_gated_single`). A throwaway message buffer and `None` store keep
        // the result out of provider context and off the handle store: this call
        // was Iris's own check, not a model tool call. The harness surfaces the
        // failure to the model as a user message instead.
        let mut display_only = Vec::new();
        record_call(
            &mut display_only,
            obs,
            None,
            &call,
            outcome,
            &provider_turn_id,
        )?;
        Ok(run)
    }

    async fn complete_turn(
        &mut self,
        unanswered_start: usize,
        hooks: TurnContextHooks<'_>,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
        steer: Option<&dyn SteeringSource>,
    ) -> Result<()> {
        let obs = hooks.observer;
        let governor = hooks.governor;
        // Start of the current unanswered user-message run (the prompt, plus any
        // injected steering/follow-up not yet answered by the provider). Cleared
        // once a provider turn commits assistant content; a cancellation before
        // that truncates exactly this run so no dangling trailing user message is
        // persisted. Replaces the old `roundtrip == 0` heuristic, which could not
        // see injected messages.
        let mut unanswered_start: Option<usize> = Some(unanswered_start);
        // Unbounded by default: the loop ends when the model stops emitting
        // tool calls (or on cancellation). An optional configured soft cap
        // ends the turn gracefully after that many round-trips.
        let mut roundtrip = 0usize;
        let mut reactive_overflow = ReactiveOverflowState::default();
        loop {
            if let Some(cap) = self.max_tool_roundtrips
                && roundtrip >= cap
            {
                tracing::warn!(cap, "tool round-trip soft cap reached; ending turn");
                // Reached the configured soft cap while the model still wants
                // to call tools. End the turn gracefully so completed tool work
                // and conversation state are preserved and the REPL keeps
                // running; this is a soft limit, not a provider failure.
                obs.on_event(AgentEvent::Notice(format!(
                    "stopped after {cap} tool round-trips; send another message to continue."
                )))?;
                obs.on_event(AgentEvent::TurnComplete)?;
                return Ok(());
            }
            if token.is_cancelled() {
                tracing::info!(roundtrips = roundtrip, "turn interrupted by user");
                // Drop any unanswered user run (the prompt and/or injected
                // steering/follow-up not yet answered) so the next turn does not
                // push two consecutive user messages (rejected by some
                // providers) or persist a dangling trailing user message.
                if let Some(start) = unanswered_start {
                    self.messages.truncate(start);
                }
                self.emit_interrupted(obs)?;
                return Ok(());
            }

            // Inject any queued steering before this provider request. Covers the
            // initial poll (the user may have typed while the turn was starting)
            // and the post-tool poll (steering queued while the prior round's
            // tools ran). Mirrors pi polling `getSteeringMessages` before each
            // assistant response.
            if let Some(src) = steer {
                self.inject_user(src.take_steering(), obs, &mut unanswered_start)?;
            }

            let provider_turn_id = self.next_provider_turn_id();
            obs.on_event(AgentEvent::ProviderTurnStarted {
                turn_id: provider_turn_id.clone(),
            })?;

            // Captured at the request-send boundary (immediately before the
            // provider stream is consumed) so the completed turn's duration and
            // time-to-first-output cover only this provider round trip, never the
            // tool execution that may run before the next one.
            let turn_start = Instant::now();
            let stream_result = match self.stream_turn(obs, token).await {
                Ok(result) => result,
                Err(error)
                    if provider_error_kind(&error)
                        == Some(ProviderErrorKind::ContextWindowExceeded) =>
                {
                    match self
                        .recover_context_overflow(
                            governor,
                            obs,
                            token,
                            roundtrip,
                            &mut reactive_overflow,
                            &mut unanswered_start,
                        )
                        .await
                    {
                        Ok(true) => continue,
                        Ok(false) => return Ok(()),
                        Err(error) => {
                            obs.on_event(AgentEvent::ProviderTurnError {
                                turn_id: provider_turn_id.clone(),
                                message: format!("{error:#}"),
                            })?;
                            if let Some(start) = unanswered_start {
                                self.messages.truncate(start);
                            }
                            return Err(error);
                        }
                    }
                }
                Err(error) => {
                    obs.on_event(AgentEvent::ProviderTurnError {
                        turn_id: provider_turn_id.clone(),
                        message: format!("{error:#}"),
                    })?;
                    if let Some(start) = unanswered_start {
                        self.messages.truncate(start);
                    }
                    return Err(error);
                }
            };

            match stream_result {
                StreamResult::Cancelled { partial, saw_delta } => {
                    // Commit any partial assistant text so the transcript stays
                    // valid (it answers the unanswered user run); otherwise drop
                    // that whole run (prompt and/or injected steering/follow-up).
                    if !partial.is_empty() {
                        if saw_delta {
                            obs.on_event(AgentEvent::AssistantTextEnd(partial.clone()))?;
                        } else {
                            obs.on_event(AgentEvent::AssistantText(partial.clone()))?;
                        }
                        self.messages.push(
                            Message::assistant(&partial).with_provider_turn_id(&provider_turn_id),
                        );
                    } else if let Some(start) = unanswered_start {
                        self.messages.truncate(start);
                    }
                    tracing::info!(
                        roundtrips = roundtrip,
                        "turn interrupted during model stream"
                    );
                    obs.on_event(AgentEvent::ProviderTurnCancelled {
                        turn_id: provider_turn_id.clone(),
                    })?;
                    self.emit_interrupted(obs)?;
                    return Ok(());
                }
                StreamResult::Completed {
                    turn,
                    saw_delta,
                    saw_reasoning_delta,
                    first_output,
                } => {
                    if turn.completion_reason == Some(CompletionReason::ContextWindowExceeded) {
                        let had_visible_content =
                            turn.text.as_deref().is_some_and(|text| !text.is_empty())
                                || !turn.reasoning.is_empty()
                                || !turn.tool_calls.is_empty()
                                || saw_delta
                                || saw_reasoning_delta;
                        let recovery = if had_visible_content {
                            Err(context_overflow_error(reactive_overflow.measurement))
                        } else {
                            self.recover_context_overflow(
                                governor,
                                obs,
                                token,
                                roundtrip,
                                &mut reactive_overflow,
                                &mut unanswered_start,
                            )
                            .await
                        };
                        match recovery {
                            Ok(true) => continue,
                            Ok(false) => return Ok(()),
                            Err(error) => {
                                obs.on_event(AgentEvent::ProviderTurnError {
                                    turn_id: provider_turn_id.clone(),
                                    message: format!("{error:#}"),
                                })?;
                                if let Some(start) = unanswered_start {
                                    self.messages.truncate(start);
                                }
                                return Err(error);
                            }
                        }
                    }
                    reactive_overflow = ReactiveOverflowState::default();
                    let AssistantTurn {
                        text,
                        reasoning,
                        tool_calls,
                        response_id,
                        usage,
                        completion_reason,
                    } = *turn;
                    // Captured before `reasoning` is consumed below: drives
                    // whether a content-less completion (e.g. a bare refusal)
                    // needs an explanatory notice.
                    let had_visible_content = text.as_deref().is_some_and(|t| !t.is_empty())
                        || !tool_calls.is_empty()
                        || !reasoning.is_empty();
                    for block in reasoning {
                        // Surface reasoning for display WITHOUT changing storage:
                        // the row is still persisted below exactly as before
                        // (ADR-0016 continuity/redacted handling is untouched).
                        // Redacted blocks never carry their text downstream.
                        //
                        // Suppression (ADR-0050): when this turn already streamed
                        // its reasoning summary live (`saw_reasoning_delta`), the
                        // front-end has shown the thinking block, so the terminal
                        // display event for the same non-redacted summary is
                        // suppressed to avoid a duplicate. Redacted blocks are
                        // never streamed, so their placeholder is always emitted.
                        // Storage below is unchanged either way (persisted once).
                        if block.redacted {
                            obs.on_event(AgentEvent::AssistantReasoning {
                                text: String::new(),
                                redacted: true,
                            })?;
                        } else if !block.text.is_empty() && !saw_reasoning_delta {
                            obs.on_event(AgentEvent::AssistantReasoning {
                                text: block.text.clone(),
                                redacted: false,
                            })?;
                        }
                        self.messages.push(
                            Message::assistant_reasoning_block(block)
                                .with_provider_turn_id(&provider_turn_id),
                        );
                    }
                    if let Some(text) = text.as_deref().filter(|text| !text.is_empty()) {
                        if saw_delta {
                            obs.on_event(AgentEvent::AssistantTextEnd(text.to_string()))?;
                        } else {
                            obs.on_event(AgentEvent::AssistantText(text.to_string()))?;
                        }
                        self.messages.push(
                            Message::assistant(text).with_provider_turn_id(&provider_turn_id),
                        );
                    } else if saw_delta {
                        obs.on_event(AgentEvent::AssistantTextEnd(String::new()))?;
                    }

                    // A completed provider turn answers the current unanswered
                    // user run: a later cancellation must not truncate it.
                    unanswered_start = None;

                    let usage_anchor = usage.clone();
                    // Measured before this round's tool calls run, so duration is
                    // this provider round trip only; TTFT is a real streamed
                    // delta's offset, or None for a non-streaming completion.
                    let timing = ProviderTurnTiming {
                        duration: turn_start.elapsed(),
                        time_to_first_output: first_output.map(|at| at - turn_start),
                    };
                    obs.on_event(AgentEvent::ProviderTurnCompleted {
                        turn_id: provider_turn_id.clone(),
                        response_id,
                        usage,
                        completion_reason,
                        timing,
                    })?;
                    // Surface notable completions (truncation, content-less
                    // refusal) to the user. Provider-neutral: driven by the
                    // typed completion metadata, not any provider-specific
                    // stop-reason string.
                    if let Some(notice) = completion_reason
                        .and_then(|reason| completion_reason_notice(reason, had_visible_content))
                    {
                        obs.on_event(AgentEvent::Notice(notice.to_string()))?;
                    }
                    if tool_calls.is_empty() {
                        self.last_provider_usage =
                            usage_anchor.map(|usage| (usage, self.messages.len()));
                        // The agent would stop here. Steering queued during this
                        // (tool-less) response runs first, then follow-up. Either
                        // keeps the loop alive with another turn; with neither,
                        // the turn is complete. Mirrors pi polling steering after
                        // every turn and follow-up only at the stop point.
                        let injected = match steer {
                            Some(src) => {
                                let steering = src.take_steering();
                                if steering.is_empty() {
                                    src.take_follow_up()
                                } else {
                                    steering
                                }
                            }
                            None => Vec::new(),
                        };
                        if injected.is_empty() {
                            tracing::debug!(roundtrips = roundtrip + 1, "turn complete");
                            obs.on_event(AgentEvent::TurnComplete)?;
                            return Ok(());
                        }
                        obs.on_messages_committed(&self.messages);
                        if !self
                            .govern_at_boundary(governor, obs, token, roundtrip + 1, true)
                            .await?
                        {
                            return Ok(());
                        }
                        self.inject_user(injected, obs, &mut unanswered_start)?;
                        // A tool-less steering/follow-up continuation is not a
                        // tool round-trip, so it must not advance the soft-cap
                        // counter: counting it could trip the cap check at the
                        // top of the next iteration and return before the
                        // provider ever answers the injected message, leaving a
                        // dangling unanswered user message. The injected turn
                        // still gets a provider response; only genuine tool
                        // rounds (below) advance the counter.
                        continue;
                    }
                    for call in &tool_calls {
                        self.messages.push(
                            Message::assistant_tool_call(call)
                                .with_provider_turn_id(&provider_turn_id),
                        );
                    }
                    // Provider usage already includes the assistant response,
                    // including tool calls. Only the tool results appended after
                    // this anchor are locally estimated by the governor.
                    self.last_provider_usage =
                        usage_anchor.map(|usage| (usage, self.messages.len()));

                    let tools_phase = self
                        .run_tools(tool_calls, obs, gate, env, token, &provider_turn_id)
                        .await?;
                    obs.on_messages_committed(&self.messages);
                    match tools_phase {
                        ToolsPhase::Ended => return Ok(()),
                        ToolsPhase::Continue => {
                            if !self
                                .govern_at_boundary(governor, obs, token, roundtrip + 1, true)
                                .await?
                            {
                                return Ok(());
                            }
                        }
                    }
                }
            }
            roundtrip += 1;
        }
    }

    /// Consult the host policy at one pair-closed continuation boundary. A
    /// governor failure is non-fatal by contract: report it and keep the user's
    /// turn moving. Cancellation wins the race and ends the turn without
    /// injecting any queued user message after the boundary.
    async fn govern_at_boundary(
        &mut self,
        governor: Option<&dyn ContextGovernor>,
        obs: &dyn AgentObserver,
        token: &CancellationToken,
        round_trip: usize,
        turn_continues: bool,
    ) -> Result<bool> {
        let Some(governor) = governor else {
            return Ok(true);
        };
        let last_usage = self
            .last_provider_usage
            .as_ref()
            .map(|(usage, message_count)| ProviderUsageAnchor {
                usage,
                message_count: *message_count,
            });
        let cx = BoundaryContext {
            messages: &self.messages,
            last_usage,
            round_trip,
            turn_continues,
        };
        let directive = tokio::select! {
            biased;
            _ = token.cancelled() => {
                self.emit_interrupted(obs)?;
                return Ok(false);
            }
            result = governor.at_boundary(cx) => result,
        };
        match directive {
            Ok(ContextDirective::Proceed) => {}
            Ok(ContextDirective::Replace { messages }) => self.replace_messages(messages),
            Err(error) => {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "context governor failed; continuing turn"
                );
                let _ = obs.on_event(AgentEvent::Notice(format!(
                    "automatic context management failed; continuing without rewriting context: {error}"
                )));
            }
        }
        Ok(true)
    }

    async fn recover_context_overflow(
        &mut self,
        governor: Option<&dyn ContextGovernor>,
        obs: &dyn AgentObserver,
        token: &CancellationToken,
        round_trip: usize,
        state: &mut ReactiveOverflowState,
        unanswered_start: &mut Option<usize>,
    ) -> Result<bool> {
        if state.attempted {
            return Err(context_overflow_error(state.measurement));
        }
        let Some(governor) = governor else {
            return Err(context_overflow_error(None));
        };
        let last_usage = self
            .last_provider_usage
            .as_ref()
            .map(|(usage, message_count)| ProviderUsageAnchor {
                usage,
                message_count: *message_count,
            });
        let cx = BoundaryContext {
            messages: &self.messages,
            last_usage,
            round_trip,
            turn_continues: true,
        };
        let recovery = tokio::select! {
            biased;
            _ = token.cancelled() => {
                self.emit_interrupted(obs)?;
                return Ok(false);
            }
            result = governor.on_context_overflow(cx) => result,
        };
        match recovery {
            Ok(ContextOverflowRecovery::Resend {
                messages,
                measured,
                effective_window,
            }) => {
                self.replace_messages(messages);
                if unanswered_start.is_some() {
                    *unanswered_start = self
                        .messages
                        .iter()
                        .rposition(|message| message.role == Role::User);
                }
                state.attempted = true;
                state.measurement = Some((measured, effective_window));
                Ok(true)
            }
            Ok(ContextOverflowRecovery::Unrecoverable {
                measured,
                effective_window,
            }) => Err(context_overflow_error(Some((measured, effective_window)))),
            Err(error) => {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "reactive context recovery failed"
                );
                Err(context_overflow_error(None))
            }
        }
    }

    /// Inject the host-queued user messages drained at one injection point
    /// (steering or follow-up) and announce them so the front-end renders the
    /// user row in transcript order. Enforces the transcript invariant "no two
    /// consecutive same-role user messages" (some providers reject them, others
    /// only coalesce on the wire): a batch drained together is merged with
    /// `\n\n`, and if the turn's trailing message is already a user message (the
    /// just-pushed prompt, or an earlier injection this turn) the merged text
    /// extends it in place rather than pushing a second user message. That
    /// trailing user is always from the current turn -- a completed turn ends on
    /// assistant content and a cancellation truncates its unanswered user run --
    /// so mutating it stays consistent with the harness's post-turn persistence
    /// diff. Sets `unanswered_start` to the start of the unanswered run so a
    /// cancellation before the provider answers truncates exactly these messages
    /// and never an answered one. The announced event carries only the newly
    /// injected text so it renders as its own row. A no-op for an empty batch,
    /// so it never spuriously starts an unanswered run.
    fn inject_user(
        &mut self,
        messages: Vec<String>,
        obs: &dyn AgentObserver,
        unanswered_start: &mut Option<usize>,
    ) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let text = messages.join("\n\n");
        if self
            .messages
            .last()
            .is_some_and(|last| last.role == Role::User)
        {
            let idx = self.messages.len() - 1;
            let last = &mut self.messages[idx];
            last.content.push_str("\n\n");
            last.content.push_str(&text);
            if unanswered_start.is_none() {
                *unanswered_start = Some(idx);
            }
        } else {
            if unanswered_start.is_none() {
                *unanswered_start = Some(self.messages.len());
            }
            self.messages.push(Message::user(&text));
        }
        obs.on_event(AgentEvent::UserMessage(text))?;
        Ok(())
    }

    /// Consume one provider stream to its terminal event, emitting text deltas
    /// and racing every read against cancellation. Borrows `&self` (messages +
    /// tools) only for the stream's lifetime; the owned [`StreamResult`] lets
    /// the caller mutate the transcript afterward.
    async fn stream_turn(
        &self,
        obs: &dyn AgentObserver,
        token: &CancellationToken,
    ) -> Result<StreamResult> {
        let mut stream = self
            .provider
            .respond_stream(&self.messages, &self.tools, token)?;
        let mut saw_delta = false;
        let mut saw_reasoning_delta = false;
        // Instant of the first non-empty streamed delta of any output channel;
        // stays `None` for a non-streaming completion. Set once, never fabricated
        // from wall-clock duration.
        let mut first_output: Option<Instant> = None;
        let mut partial = String::new();
        loop {
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    return Ok(StreamResult::Cancelled { partial, saw_delta });
                }
                item = stream.next() => match item {
                    Some(Ok(ProviderEvent::TextDelta(delta))) => {
                        saw_delta = true;
                        if !delta.is_empty() {
                            first_output.get_or_insert_with(Instant::now);
                        }
                        partial.push_str(&delta);
                        obs.on_event(AgentEvent::AssistantTextDelta(delta))?;
                    }
                    Some(Ok(ProviderEvent::ReasoningDelta(delta))) => {
                        // Display-only: never accumulated into `partial` (which
                        // becomes the persisted assistant text) or into storage.
                        if !delta.is_empty() {
                            saw_reasoning_delta = true;
                            first_output.get_or_insert_with(Instant::now);
                            obs.on_event(AgentEvent::AssistantReasoningDelta(delta))?;
                        }
                    }
                    Some(Ok(ProviderEvent::RawReasoningDelta(delta))) => {
                        // Display-only and kept on a distinct raw channel; never
                        // accumulated into `partial` or storage.
                        if !delta.is_empty() {
                            saw_reasoning_delta = true;
                            first_output.get_or_insert_with(Instant::now);
                            obs.on_event(AgentEvent::AssistantRawReasoningDelta(delta))?;
                        }
                    }
                    Some(Ok(ProviderEvent::ReasoningSectionBreak)) => {
                        obs.on_event(AgentEvent::AssistantReasoningSectionBreak)?;
                    }
                    Some(Ok(ProviderEvent::ToolInputDelta { call_id, delta })) => {
                        // Display-only (ADR-0039): forwarded for a live preview
                        // but NEVER accumulated into `partial` (the persisted
                        // assistant text), `self.messages`, or the assembled
                        // turn's tool calls. Approval and execution use only the
                        // completed `ToolCall`, so these deltas cannot change what
                        // runs even if tampered with or dropped.
                        if !delta.is_empty() {
                            first_output.get_or_insert_with(Instant::now);
                        }
                        obs.on_event(AgentEvent::ToolInputDelta { call_id, delta })?;
                    }
                    Some(Ok(ProviderEvent::Activity)) => {}
                    Some(Ok(ProviderEvent::Completed(turn))) => {
                        return Ok(StreamResult::Completed {
                            turn: Box::new(turn),
                            saw_delta,
                            saw_reasoning_delta,
                            first_output,
                        });
                    }
                    Some(Err(error)) => return Err(error),
                    None => bail!("provider stream closed before completion"),
                },
            }
        }
    }

    /// Execute the model's tool calls: consecutive concurrency-safe calls run in
    /// parallel, every other call runs exclusively (one at a time). Transcript
    /// order is preserved regardless of completion order. On cancellation, every
    /// not-yet-executed call still gets a synthetic cancelled result so the next
    /// model request stays valid.
    async fn run_tools(
        &mut self,
        calls: Vec<ToolCall>,
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
        provider_turn_id: &str,
    ) -> Result<ToolsPhase> {
        let store = env.output_store;
        let mut idx = 0;
        while idx < calls.len() {
            if token.is_cancelled() {
                tracing::info!(
                    pending = calls.len() - idx,
                    "turn interrupted during tools; remaining calls cancelled"
                );
                for call in &calls[idx..] {
                    emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Proposed)?;
                    record_call(
                        &mut self.messages,
                        obs,
                        store,
                        call,
                        ToolOutcome::Cancelled,
                        provider_turn_id,
                    )?;
                }
                self.emit_interrupted(obs)?;
                return Ok(ToolsPhase::Ended);
            }

            if self.is_parallelizable(&calls[idx]) {
                let mut end = idx;
                while end < calls.len() && self.is_parallelizable(&calls[end]) {
                    end += 1;
                }
                for call in &calls[idx..end] {
                    obs.on_event(AgentEvent::ToolProposed(call.clone()))?;
                    emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Proposed)?;
                    obs.on_event(AgentEvent::ToolStarted(call.clone()))?;
                    emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Started)?;
                }
                // Scope the borrow of `self.tools` so it drops before the
                // transcript pushes below.
                let outcomes = run_parallel(&self.tools, &calls[idx..end], env, token).await;
                for (call, outcome) in calls[idx..end].iter().zip(outcomes) {
                    record_call(
                        &mut self.messages,
                        obs,
                        store,
                        call,
                        outcome,
                        provider_turn_id,
                    )?;
                }
                idx = end;
            } else {
                let outcome = self
                    .run_gated_single(&calls[idx], obs, gate, env, token, provider_turn_id)
                    .await?;
                record_call(
                    &mut self.messages,
                    obs,
                    store,
                    &calls[idx],
                    outcome,
                    provider_turn_id,
                )?;
                idx += 1;
            }
        }

        // The model gets another round-trip to react to the tool results; the
        // turn is not complete here (only an empty tool-call response, the
        // round-trip cap, or a cancellation ends it).
        Ok(ToolsPhase::Continue)
    }

    /// Whether a call may join a parallel batch: it resolves to a known tool
    /// that is concurrency-safe and ungated. Gated tools always take the
    /// exclusive path so their approval prompt runs alone.
    fn is_parallelizable(&self, call: &ToolCall) -> bool {
        self.tools.by_name(&call.name).is_some_and(|tool| {
            tool.is_concurrency_safe()
                && !tool.requires_approval()
                && !tool.requires_user_interaction()
        })
    }

    /// The exclusive (default) path for one call: approval policy, then a single
    /// cancellation-raced execution. Returns the outcome; the caller records it.
    async fn run_gated_single(
        &mut self,
        call: &ToolCall,
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
        provider_turn_id: &str,
    ) -> Result<ToolOutcome> {
        emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Proposed)?;
        // Dirty-tree safety (issue #262, ADR-0028). A mutating call lazily opens
        // the task baseline (surfaced once) and, when it targets a statically
        // known set of paths, is checked against the dirty baseline. A protected
        // path not yet approved this task must route through the approval gate
        // even if a session/project allow layer would auto-run it. Nexus owns
        // this enforcement; the git knowledge lives behind the guard seam.
        let (is_mutating, mutated_paths) = self
            .tools
            .by_name(&call.name)
            .map(|tool| (tool.is_mutating(), tool.mutates_paths(&call.arguments)))
            .unwrap_or((false, Vec::new()));
        let dirty_protected: Vec<PathBuf> = if let Some(guard) = env.mutation_guard {
            if is_mutating && let Some(summary) = guard.note_mutation() {
                obs.on_event(AgentEvent::DirtyBaseline(summary))?;
            }
            guard.unapproved_protected(&mutated_paths)
        } else {
            Vec::new()
        };
        let dirty_gate = !dirty_protected.is_empty();
        // Workspace-relative display paths for the dirty-tree gate, computed
        // once: reused by the Notice (transcript record) and threaded to the
        // front-end via `ReviewContext` for the decision-point reason line.
        let dirty_display: Vec<String> = dirty_protected
            .iter()
            .map(|path| crate::display_path::workspace_path(env.workspace, path))
            .collect();
        let mut interaction_arguments = None;
        if let Some(tool) = self.tools.by_name(&call.name) {
            if let Some(diff) = tool.diff_preview(env.workspace, &call.arguments) {
                obs.on_event(AgentEvent::DiffPreview {
                    call: call.clone(),
                    diff,
                })?;
            }
            if tool.requires_user_interaction() {
                // Required interaction is not approval: every permission mode,
                // including never-ask and dangerous skip-permissions, must park
                // here until the user submits, rejects, or cancels the turn.
                obs.on_event(AgentEvent::ToolProposed(call.clone()))?;
                let interaction = tokio::select! {
                    biased;
                    _ = token.cancelled() => return Ok(ToolOutcome::Cancelled),
                    outcome = gate.interact(call) => outcome?,
                };
                if token.is_cancelled() {
                    return Ok(ToolOutcome::Cancelled);
                }
                match interaction {
                    InteractionOutcome::Submitted(arguments) => {
                        interaction_arguments = Some(arguments);
                    }
                    InteractionOutcome::Rejected { feedback: None } => {
                        return Ok(ToolOutcome::Denied);
                    }
                    InteractionOutcome::Rejected {
                        feedback: Some(feedback),
                    } => {
                        return Ok(ToolOutcome::DeniedWithFeedback(
                            truncate_interaction_feedback(feedback),
                        ));
                    }
                }
            } else if !tool.requires_approval() && !dirty_gate {
                obs.on_event(AgentEvent::ToolProposed(call.clone()))?;
            } else if self.skip_permissions {
                // `--dangerously-skip-permissions` (ADR-0049): the operator has
                // taken responsibility for every effect this session. Bypass the
                // approval gate for this gated call BEFORE any floor, grant, or
                // preset is consulted -- including the destructive and dirty-tree
                // floors that normally re-prompt. This is the single skip check
                // at the top of the approval decision path; the floor/preset
                // logic below is left untouched for every non-skip session.
                //
                // Non-persistence (invariant): nothing is written to
                // `session_allowed` or the project `policy_sink` here, so a
                // skip-mode session leaves no persistent allow behind. The audit
                // is a distinct, greppable event so the bypass is never silent.
                obs.on_event(AgentEvent::ToolAutoApprovedDangerous(call.clone()))?;
                emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Approved)?;
            } else {
                // A dirty-tree gate (issue #262) forces the approval path even
                // for a tool that does not otherwise require approval: a
                // mutating tool with `requires_approval() == false` but a
                // protected `mutates_paths()` target must never skip the gate.
                // The `auto_approved` computation below keeps `dirty_gate` a hard
                // floor, so entering this branch always prompts for such a tool.
                // The destructive floor (ADR-0010): a destructive call (e.g.
                // `rm`) always re-prompts, before any allow layer is consulted.
                // Neither a session allow-always nor a persistent project grant
                // (ADR-0027) can silently auto-run a data-losing command.
                let destructive = tool.is_destructive(&call.arguments);
                let session_allowed =
                    self.session_allowed.contains(&call.name) && tool.supports_allow_always();
                // Precedence session > project > global default (prompt). Every
                // layer is allow-only, so "most specific wins" reduces to a
                // union of the allow layers over the prompting default.
                let project_allowed = self.project_policy.allows(&call.name, &call.arguments);
                // A dirty-tree gate (issue #262) sits above every allow layer:
                // touching a pre-existing dirty file always prompts this task,
                // no matter what the session/project policy grants. Like the
                // destructive floor, it cannot be silently auto-run.
                //
                // ADR-0032 floors: a destructive command or a protected dirty
                // target prevents silent execution before any grant or preset is
                // consulted. (The sandbox floor lives in the tool's
                // `auto_approvable`, which returns false for `bash` in v1, so
                // unproven sandboxed bash never auto-runs.)
                let blocked_by_floor = destructive || dirty_gate;
                let explicit_allowed = session_allowed || project_allowed;
                // `auto` preset: additionally auto-run a call the deterministic
                // auto policy proves safe. Consulted only in Auto mode and only
                // after the floors, so it is a classification input, never an
                // authority (ADR-0014). Explicit grants (`explicit_allowed`) run
                // in every mode -- including `never` -- because they are not a
                // prompt; the auto preset does not.
                let auto_allowed = self.approval_mode == ApprovalMode::Auto
                    && tool.auto_approvable(env.workspace, &call.arguments);
                let auto_approved = !blocked_by_floor && (explicit_allowed || auto_allowed);
                if auto_approved {
                    obs.on_event(AgentEvent::ToolAutoApproved(call.clone()))?;
                    emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Approved)?;
                }
                if !auto_approved {
                    if destructive && (session_allowed || project_allowed) {
                        let message = "destructive command: approval required even though this tool is allowed";
                        obs.on_event(AgentEvent::Notice(message.to_string()))?;
                    }
                    if dirty_gate {
                        let list = dirty_display.join(", ");
                        obs.on_event(AgentEvent::Notice(format!(
                            "uncommitted changes in {list}: approval required before Iris modifies it"
                        )))?;
                    }
                    // Never-ask (ADR-0032): a call that reaches the prompt path
                    // is an unresolved prompt -- deny it instead of asking. An
                    // explicit non-floor session/project grant already set
                    // `auto_approved` above, so it never lands here; only a
                    // floor-blocked call or an ungranted call does. Return a
                    // normal denied result so the model sees a refusal, not a
                    // silent bypass.
                    if self.approval_mode == ApprovalMode::NeverAsk {
                        tracing::info!(
                            tool = %call.name,
                            "tool call denied by never-ask mode (prompt suppressed)"
                        );
                        obs.on_event(AgentEvent::Notice(format!(
                            "never-ask mode: `{}` would prompt for approval; denied",
                            call.name
                        )))?;
                        return Ok(ToolOutcome::Denied);
                    }
                    // Facts, not copy: the destructive floor and the dirty-tree
                    // paths cross to the front-end so it can render the reason
                    // line at the decision point (docs/ARCHITECTURE.md).
                    let review_ctx = ReviewContext {
                        destructive,
                        dirty_paths: dirty_display.clone(),
                    };
                    // Race the approval against cancellation so a pending prompt
                    // does not pin the turn open after a Ctrl-C. Cancellation is
                    // recorded as a cancelled call (not a denial) so the transcript
                    // reflects user intent rather than a refusal.
                    emit_tool_lifecycle(
                        obs,
                        provider_turn_id,
                        call,
                        ToolEventState::ApprovalRequested,
                    )?;
                    let decision = tokio::select! {
                        biased;
                        _ = token.cancelled() => return Ok(ToolOutcome::Cancelled),
                        // A project grant is never offered for a destructive
                        // call: it must not be persistable (ADR-0010 floor). A
                        // dirty-tree gate offers the "all dirty files (this task)"
                        // escalation via allow-always and suppresses the project
                        // grant (a dirty file cannot be pre-approved for a
                        // project).
                        decision = gate.review(
                            call,
                            tool.supports_allow_always() || dirty_gate,
                            !destructive && !dirty_gate,
                            review_ctx,
                        ) => decision?,
                    };
                    // A blocking front-end prompt (real terminal) cannot observe
                    // the token mid-read, so it may still return a decision after a
                    // Ctrl-C landed. Treat the turn cancellation as authoritative so
                    // a late Allow/Deny neither runs the tool nor mutates the
                    // session allow-policy.
                    if token.is_cancelled() {
                        return Ok(ToolOutcome::Cancelled);
                    }
                    match decision {
                        ApprovalDecision::Deny => {
                            tracing::warn!(tool = %call.name, "tool call denied by user");
                            return Ok(ToolOutcome::Denied);
                        }
                        ApprovalDecision::AllowAlways => {
                            if dirty_gate {
                                // In the dirty-tree context "always" means "all
                                // dirty files this task" (ADR-0028 escalation),
                                // not a persistent session grant for the tool.
                                if let Some(guard) = env.mutation_guard {
                                    guard.approve(&dirty_protected, true);
                                }
                            } else if tool.supports_allow_always() {
                                tracing::info!(tool = %call.name, "tool always-allowed this session");
                                self.session_allowed.insert(call.name.clone());
                            } else {
                                let message = format!(
                                    "always-allow is disabled for `{}`; it requires approval each time.",
                                    call.name
                                );
                                obs.on_event(AgentEvent::Notice(message))?;
                            }
                        }
                        ApprovalDecision::AllowProject => {
                            // Defense in depth: even if a front-end returns
                            // AllowProject for a destructive call (it was not
                            // offered), the grant is refused -- the call still
                            // runs once, but nothing is persisted (invariant 2).
                            if dirty_gate {
                                // A dirty file is never project-granted; approve
                                // it for this task only.
                                if let Some(guard) = env.mutation_guard {
                                    guard.approve(&dirty_protected, false);
                                }
                            } else if destructive {
                                let message = "destructive command: cannot be granted for this project; allowed once";
                                obs.on_event(AgentEvent::Notice(message.to_string()))?;
                            } else {
                                self.record_project_grant(call, obs)?;
                            }
                        }
                        ApprovalDecision::Allow => {
                            if dirty_gate {
                                // Approve just the dirty paths this call touches,
                                // for the remainder of the task.
                                if let Some(guard) = env.mutation_guard {
                                    guard.approve(&dirty_protected, false);
                                }
                            }
                        }
                    }
                    emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Approved)?;
                }
            }
        } else {
            obs.on_event(AgentEvent::ToolProposed(call.clone()))?;
        }

        // Resolve again for execution (the approval borrow above has ended); an
        // unknown tool yields the same `unknown tool: <name>` result as before.
        let outcome = match self.tools.by_name(&call.name) {
            Some(tool) => {
                // Dirty-tree detection layer (issue #262, ADR-0028): a preflight
                // halt prevents execution; otherwise snapshot protected paths and
                // re-check them after the call.
                let guard = env.mutation_guard.filter(|_| is_mutating);
                if let Some(guard) = guard {
                    let violation = guard.before_exec(&mutated_paths);
                    if !violation.is_empty() {
                        return guard_violation_outcome(
                            guard,
                            violation,
                            call,
                            env.workspace,
                            obs,
                            false,
                        );
                    }
                }

                // Announce execution only after preflight succeeds, then inject a
                // per-call streaming emitter. Unknown and preflight-halted tools
                // never open a phantom live cell.
                obs.on_event(AgentEvent::ToolStarted(call.clone()))?;
                emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Started)?;
                let emitter = ToolDeltaEmitter {
                    obs,
                    call_id: call.id.clone(),
                };
                let call_env = ToolEnv {
                    workspace: env.workspace,
                    state: env.state,
                    output_store: env.output_store,
                    session_span: env.session_span,
                    output_sink: Some(&emitter),
                    mutation_guard: env.mutation_guard,
                };
                let execution_arguments = interaction_arguments.as_ref().unwrap_or(&call.arguments);
                let outcome =
                    run_tool(tool, execution_arguments, &call_env, token.child_token()).await;
                if let Some(guard) = guard {
                    // Confirm an approved change against the exact bytes the tool
                    // reported writing (ADR-0028 TOCTOU rule). A failed/cancelled
                    // call, or a tool that reports no hash, yields `None`, so any
                    // change to a protected file stays user-attributed. The key
                    // is stripped from provider context in `record_call`.
                    let expected_after = match &outcome {
                        ToolOutcome::Ok(output) => output
                            .metadata
                            .get(WRITE_CONFIRM_HASH_KEY)
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        _ => None,
                    };
                    let violation = guard.after_exec(&mutated_paths, expected_after.as_deref());
                    if violation.is_empty() {
                        outcome
                    } else {
                        guard_violation_outcome(guard, violation, call, env.workspace, obs, true)?
                    }
                } else {
                    outcome
                }
            }
            None => ToolOutcome::Err(anyhow::anyhow!("unknown tool: {}", call.name)),
        };
        // Record that Iris changed the workspace this turn when a mutating tool's
        // FINAL outcome is Ok (a guard-halted/restored call ends as Err and does
        // not count). Drives the harness's post-change verification trigger
        // (issue #265). Uses the post-guard outcome so a violation that restored
        // the file is not mistaken for a real mutation.
        if is_mutating && matches!(outcome, ToolOutcome::Ok(_)) {
            self.mutated_this_turn = true;
        }
        Ok(outcome)
    }

    /// Apply and persist a per-project grant derived from an approved,
    /// non-destructive call (ADR-0027): the exact command for `bash`, the tool
    /// name otherwise. The in-memory policy always updates (this session honors
    /// the grant immediately); a missing sink or a persistence failure is
    /// reported and degrades to session-lifetime scope, never blocks the call.
    fn record_project_grant(&mut self, call: &ToolCall, obs: &dyn AgentObserver) -> Result<()> {
        let grant = if call.name == "bash" {
            match call.arguments.get("command").and_then(Value::as_str) {
                Some(command) if !command.trim().is_empty() => {
                    PolicyGrant::BashExact(command.trim().to_string())
                }
                _ => {
                    let message =
                        "could not derive a project grant from this call; allowed once".to_string();
                    return obs.on_event(AgentEvent::Notice(message));
                }
            }
        } else {
            PolicyGrant::Tool(call.name.clone())
        };
        self.project_policy.apply(&grant);
        let notice = match &self.policy_sink {
            Some(sink) => match sink.persist(&grant) {
                Ok(()) => match &grant {
                    PolicyGrant::Tool(name) => {
                        format!("`{name}` is now always allowed for this project")
                    }
                    PolicyGrant::BashExact(command) => {
                        format!("`{command}` is now always allowed for this project")
                    }
                },
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "failed to persist project grant");
                    format!(
                        "could not save the project grant ({error:#}); it applies to this session only"
                    )
                }
            },
            None => "project grants are not persisted in this run; allowed for this session only"
                .to_string(),
        };
        obs.on_event(AgentEvent::Notice(notice))
    }

    fn emit_interrupted(&self, obs: &dyn AgentObserver) -> Result<()> {
        obs.on_event(AgentEvent::Notice(INTERRUPT_NOTICE.to_string()))?;
        obs.on_event(AgentEvent::TurnComplete)
    }

    fn next_provider_turn_id(&mut self) -> String {
        let id = format!("turn_{:08x}", self.next_provider_turn_seq);
        self.next_provider_turn_seq += 1;
        id
    }
}

/// Per-call streaming emitter injected into a tool's [`ToolEnv`] on the
/// exclusive execution path. Forwards each chunk the tool produces to the
/// observer as a display-only [`AgentEvent::ToolOutputDelta`] tagged with the
/// call id, so the front-end attaches the live output to the right cell. The
/// observer error is intentionally swallowed: a streamed-progress delivery
/// failure must not abort the tool, whose final result still flows through
/// [`record_call`].
struct ToolDeltaEmitter<'a> {
    obs: &'a dyn AgentObserver,
    call_id: String,
}

impl ToolOutputSink for ToolDeltaEmitter<'_> {
    fn emit_chunk(&self, chunk: &str) {
        let _ = self.obs.on_event(AgentEvent::ToolOutputDelta {
            call_id: self.call_id.clone(),
            chunk: chunk.to_string(),
        });
    }
}

/// Run a bounded batch of concurrency-safe calls concurrently, returning outcomes
/// in the same order as `calls`. Each call gets its own child cancellation token.
/// Uses ordered buffering (not `tokio::spawn`) so the `!Send` borrowed futures run
/// on the loop's executor without queuing unbounded blocking work.
async fn run_parallel(
    tools: &Tools,
    calls: &[ToolCall],
    env: &ToolEnv<'_>,
    token: &CancellationToken,
) -> Vec<ToolOutcome> {
    futures::stream::iter(calls.iter())
        .map(|call| {
            let cancel = token.child_token();
            async move {
                match tools.by_name(&call.name) {
                    Some(tool) => run_tool(tool, &call.arguments, env, cancel).await,
                    None => ToolOutcome::Err(anyhow::anyhow!("unknown tool: {}", call.name)),
                }
            }
        })
        // No fixed parallelism cap: every parallelizable call in the batch runs
        // concurrently (matching pi-mono's `Promise.all` over the batch).
        // `buffered` (not `buffer_unordered`) preserves result order, which the
        // caller zips back onto the calls. `max(1)` guards the empty-batch case
        // (`buffered(0)` would stall); run_tools only calls this with a
        // non-empty parallelizable run.
        .buffered(calls.len().max(1))
        .collect()
        .await
}

fn truncate_interaction_feedback(mut feedback: String) -> String {
    if feedback.len() > MAX_INTERACTION_FEEDBACK_BYTES {
        let boundary = feedback.floor_char_boundary(MAX_INTERACTION_FEEDBACK_BYTES);
        feedback.truncate(boundary);
    }
    feedback
}

/// Run one tool, racing its future against the (child) cancellation token. The
/// pre-check matters: a synchronous tool body would otherwise run to completion
/// on the first poll even when already cancelled (the select is `biased` toward
/// the tool so a cooperative tool's own result wins over the synthetic one). The
/// post-check maps sync tools that observe cancellation internally to the same
/// transcript-valid cancelled outcome.
async fn run_tool<'a>(
    tool: &'a dyn Tool,
    args: &'a Value,
    env: &'a ToolEnv<'_>,
    cancel: CancellationToken,
) -> ToolOutcome {
    if cancel.is_cancelled() {
        return ToolOutcome::Cancelled;
    }
    tokio::select! {
        biased;
        result = tool.execute(args, env, cancel.clone()) => match result {
            _ if cancel.is_cancelled() => ToolOutcome::Cancelled,
            Ok(output) => ToolOutcome::Ok(output),
            Err(error) => ToolOutcome::Err(error),
        },
        _ = cancel.cancelled() => ToolOutcome::Cancelled,
    }
}

fn emit_tool_lifecycle(
    obs: &dyn AgentObserver,
    provider_turn_id: &str,
    call: &ToolCall,
    state: ToolEventState,
) -> Result<()> {
    obs.on_event(AgentEvent::ToolLifecycle {
        provider_turn_id: provider_turn_id.to_string(),
        call_id: call.id.clone(),
        name: call.name.clone(),
        state,
    })
}

/// Stable host metadata for an oversized tool output stored out of provider
/// context. This is safe to persist and emit because it carries only the handle
/// id and already-computed size/count data, never the full body or preview.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OutputHandleMetadata {
    id: String,
    bytes: usize,
    lines: usize,
}

impl OutputHandleMetadata {
    fn to_value(&self) -> Value {
        json!({ "id": self.id, "bytes": self.bytes, "lines": self.lines })
    }
}

/// Provider-neutral model-facing tool-result envelope. Concrete tools own the
/// `content` text and optional metadata fields; Nexus owns how those values are
/// serialized into provider-visible success/error/denied/cancelled results.
///
/// Keep this narrow and Rust-native: it documents and enforces today's wire
/// shape without adding Flue-style schema generation or a public plugin API.
#[derive(Debug)]
enum ToolResultContract {
    Success(ToolOutput),
    ToolError(Error),
    Denied(Option<String>),
    Cancelled,
}

impl ToolResultContract {
    fn success(output: ToolOutput) -> Self {
        Self::Success(output)
    }

    fn tool_error(error: Error) -> Self {
        Self::ToolError(error)
    }

    fn denied() -> Self {
        Self::Denied(None)
    }

    fn denied_with_feedback(feedback: String) -> Self {
        Self::Denied(Some(feedback))
    }

    fn cancelled() -> Self {
        Self::Cancelled
    }

    fn into_wire_json(self) -> String {
        self.into_wire_value().to_string()
    }

    fn into_wire_value(self) -> Value {
        match self {
            Self::Success(output) => {
                let mut obj = serde_json::Map::new();
                obj.insert("ok".to_string(), Value::Bool(true));
                obj.insert("content".to_string(), Value::String(output.content));
                if !output.metadata.is_empty() {
                    obj.insert("metadata".to_string(), Value::Object(output.metadata));
                }
                Value::Object(obj)
            }
            Self::ToolError(error) => {
                let mut obj = serde_json::Map::new();
                obj.insert("ok".to_string(), Value::Bool(false));
                obj.insert("error".to_string(), Value::String(error.to_string()));
                // Opt-in machine-readable classification: only present when the
                // tool returned a `ClassifiedError`. Unclassified errors stay
                // byte-identical to `{ "ok": false, "error": ... }`. This mirrors
                // the Denied/Cancelled precedent of adding a flag beside `error`.
                if let Some(classified) = error.downcast_ref::<ClassifiedError>() {
                    obj.insert(
                        "metadata".to_string(),
                        Value::Object(classified.to_metadata()),
                    );
                }
                Value::Object(obj)
            }
            Self::Denied(feedback) => {
                let mut value = json!({
                    "ok": false,
                    "error": "tool call denied by user",
                    "denied": true
                });
                if let Some(feedback) = feedback {
                    value["feedback"] = Value::String(feedback);
                }
                value
            }
            Self::Cancelled => json!({
                "ok": false,
                "error": "tool call cancelled by user",
                "cancelled": true
            }),
        }
    }
}

/// Append one tool call and its result to the transcript and emit the matching
/// event. Every model-emitted call goes through here exactly once, so the
/// assistant-tool-call / tool-result pairing is always complete.
fn record_call(
    messages: &mut Vec<Message>,
    obs: &dyn AgentObserver,
    store: Option<&dyn ToolOutputStore>,
    call: &ToolCall,
    outcome: ToolOutcome,
    provider_turn_id: &str,
) -> Result<()> {
    // Append the tool result BEFORE emitting the observer event. An observer
    // error must not skip the provider-visible result for a call that already
    // ran; Wayland persists the transcript even on a turn error.
    let event = match outcome {
        ToolOutcome::Ok(mut output) => {
            tracing::info!(tool = %call.name, ok = true, "tool executed");
            // The observer still receives the full output: offloading only keeps
            // the oversized payload out of provider context, never out of the
            // user-facing display (which folds it to a preview itself).
            let content = output.content.clone();
            // Lift the display-only exec metadata out of the output BEFORE
            // serialization so the non-deterministic duration and the exit code
            // ride the event, never entering provider context (and the wire
            // shape stays byte-identical to a plain text-only result).
            // The dirty-tree write-confirmation hash (ADR-0028) is an internal
            // signal consumed in `run_gated_single`; strip it here so it never
            // enters provider context (the wire shape stays identical).
            output.metadata.remove(WRITE_CONFIRM_HASH_KEY);
            let exit_code = output
                .metadata
                .remove("exitCode")
                .and_then(|value| value.as_i64())
                .map(|code| code as i32);
            let duration = output
                .metadata
                .remove("durationMs")
                .and_then(|value| value.as_u64())
                .map(Duration::from_millis);
            let (result_json, handle) = success_tool_result_json(store, output);
            messages.push(
                Message::tool_result(&call.id, &call.name, &result_json)
                    .with_provider_turn_id(provider_turn_id),
            );
            if let Some(handle) = handle {
                obs.on_event(AgentEvent::OutputHandleStored {
                    provider_turn_id: provider_turn_id.to_string(),
                    call_id: call.id.clone(),
                    handle_id: handle.id,
                    bytes: handle.bytes,
                    lines: handle.lines,
                })?;
            }
            emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Succeeded)?;
            AgentEvent::ToolResult {
                call: call.clone(),
                content,
                exit_code,
                duration,
            }
        }
        ToolOutcome::Err(error) => {
            tracing::info!(tool = %call.name, ok = false, "tool executed");
            let message = format!("{error:#}");
            messages.push(
                Message::tool_result(
                    &call.id,
                    &call.name,
                    &ToolResultContract::tool_error(error).into_wire_json(),
                )
                .with_provider_turn_id(provider_turn_id),
            );
            emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Errored)?;
            AgentEvent::ToolError {
                call: call.clone(),
                message,
            }
        }
        ToolOutcome::Cancelled => {
            tracing::info!(tool = %call.name, "tool cancelled");
            messages.push(
                Message::tool_result(
                    &call.id,
                    &call.name,
                    &ToolResultContract::cancelled().into_wire_json(),
                )
                .with_provider_turn_id(provider_turn_id),
            );
            emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Cancelled)?;
            AgentEvent::ToolCancelled(call.clone())
        }
        ToolOutcome::Denied => {
            tracing::warn!(tool = %call.name, "tool call denied");
            messages.push(
                Message::tool_result(
                    &call.id,
                    &call.name,
                    &ToolResultContract::denied().into_wire_json(),
                )
                .with_provider_turn_id(provider_turn_id),
            );
            emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Denied)?;
            AgentEvent::ToolDenied(call.clone())
        }
        ToolOutcome::DeniedWithFeedback(feedback) => {
            tracing::warn!(tool = %call.name, "tool call denied with feedback");
            messages.push(
                Message::tool_result(
                    &call.id,
                    &call.name,
                    &ToolResultContract::denied_with_feedback(feedback).into_wire_json(),
                )
                .with_provider_turn_id(provider_turn_id),
            );
            emit_tool_lifecycle(obs, provider_turn_id, call, ToolEventState::Denied)?;
            AgentEvent::ToolDenied(call.clone())
        }
    };
    obs.on_event(event)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderUsage {
    pub(crate) provider: String,
    pub(crate) model: String,
    /// Total provider-visible input tokens, including cache reads/writes when
    /// the provider reports them separately.
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    /// Input tokens served from prompt cache. Already included in
    /// `input_tokens`; callers must not add them again when computing total
    /// input.
    pub(crate) cache_read_input_tokens: u64,
    pub(crate) cache_write_input_tokens: u64,
    pub(crate) reasoning_output_tokens: u64,
    pub(crate) total_tokens: u64,
    /// Provider breakdown of cache-creation (write) tokens by retention class,
    /// when reported. Anthropic surfaces this as `usage.cache_creation`
    /// (`ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens`); providers
    /// that do not report a breakdown leave this `None`. The component totals
    /// are already summed into `cache_write_input_tokens`.
    pub(crate) cache_creation: Option<CacheCreation>,
}

/// Wall-clock timing for one completed provider round trip. `duration` measures
/// from the provider request send to the terminal stream event (this round trip
/// only; it excludes tool execution that runs between round trips).
/// `time_to_first_output` measures from the same start to the FIRST non-empty
/// streamed output delta (assistant text delta, reasoning delta, or tool-call
/// argument delta); it is `None` when the provider completed without streaming
/// any non-empty delta (e.g. a non-streaming response). TTFT is never fabricated
/// from `duration` -- only a real streamed delta sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderTurnTiming {
    pub(crate) duration: Duration,
    pub(crate) time_to_first_output: Option<Duration>,
}

impl ProviderTurnTiming {
    /// Deterministic fixture for UI/event tests that need a timing value but do
    /// not assert on its contents. Not compiled into release builds.
    #[cfg(test)]
    pub(crate) fn sample() -> Self {
        Self {
            duration: Duration::from_millis(1200),
            time_to_first_output: Some(Duration::from_millis(300)),
        }
    }
}

/// Per-retention breakdown of prompt-cache-creation (write) input tokens, as
/// reported by Anthropic's `usage.cache_creation` detail. Surfaced alongside
/// the `cache_write_input_tokens` total so diagnostics can attribute writes to
/// the 5-minute vs 1-hour cache tier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CacheCreation {
    pub(crate) ephemeral_5m_input_tokens: u64,
    pub(crate) ephemeral_1h_input_tokens: u64,
}

/// Provider-neutral reason a model turn completed, mapped from the provider's
/// terminal stop signal (e.g. Anthropic `message_delta.delta.stop_reason`).
/// Carried as safe completion metadata on [`AssistantTurn`] and
/// [`AgentEvent::ProviderTurnCompleted`]; every variant is an enumerated
/// classification and never carries response text. Stop reasons Iris does not
/// model yet map to `Other` (the raw wire token is logged at parse time rather
/// than stored, keeping this a small `Copy` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionReason {
    /// Natural end of the assistant turn.
    EndTurn,
    /// The model stopped to call one or more tools.
    ToolUse,
    /// Output was truncated at the requested max output-token ceiling.
    MaxOutputTokens,
    /// The model's input context window was exceeded.
    ContextWindowExceeded,
    /// A configured stop sequence was emitted.
    StopSequence,
    /// The provider paused a long-running turn and expects continuation.
    Paused,
    /// The model declined to continue (safety refusal).
    Refusal,
    /// A stop reason Iris does not model yet. The raw enumerated wire token is
    /// logged at parse time rather than stored.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderErrorKind {
    ContextWindowExceeded,
}

#[derive(Debug)]
pub(crate) struct ProviderFailure {
    kind: ProviderErrorKind,
    diagnostic: String,
}

impl ProviderFailure {
    pub(crate) fn new(kind: ProviderErrorKind, diagnostic: impl Into<String>) -> Self {
        Self {
            kind,
            diagnostic: diagnostic.into(),
        }
    }

    pub(crate) fn kind(&self) -> ProviderErrorKind {
        self.kind
    }
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.diagnostic)
    }
}

impl std::error::Error for ProviderFailure {}

pub(crate) fn provider_error_kind(error: &anyhow::Error) -> Option<ProviderErrorKind> {
    error
        .downcast_ref::<ProviderFailure>()
        .map(ProviderFailure::kind)
}

/// Provider-neutral, user-facing notice for a completion reason the user should
/// know about. Returns `None` for routine reasons that need no notice. Wording
/// stays model-agnostic so it is correct for any provider; the runtime never
/// hard-codes a provider/model name here.
///
/// `had_visible_content` is whether the turn produced any text, reasoning, or
/// tool calls: a refusal that carried an explanation needs no extra notice (the
/// user already sees why), but a content-less refusal would otherwise be silent.
pub(crate) fn completion_reason_notice(
    reason: CompletionReason,
    had_visible_content: bool,
) -> Option<&'static str> {
    match reason {
        CompletionReason::MaxOutputTokens => {
            Some("The model hit its maximum output-token limit; this response may be truncated.")
        }
        CompletionReason::ContextWindowExceeded => {
            Some("The model reached its context-window limit; this response may be truncated.")
        }
        CompletionReason::Refusal if !had_visible_content => Some("The model declined to respond."),
        _ => None,
    }
}

fn context_overflow_error(measurement: Option<(u64, u64)>) -> anyhow::Error {
    match measurement {
        Some((measured, effective_window)) if effective_window > 0 => anyhow::anyhow!(
            "provider rejected context after bounded reactive recovery: measured ~{measured} tokens against a {effective_window}-token window; try `/compact <focus>`, `/new`, or switch model"
        ),
        _ => anyhow::anyhow!(
            "provider rejected context and deterministic recovery could not make it fit; try `/compact <focus>`, `/new`, or switch model"
        ),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AssistantTurn {
    pub(crate) text: Option<String>,
    pub(crate) reasoning: Vec<ReasoningBlock>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) response_id: Option<String>,
    pub(crate) usage: Option<ProviderUsage>,
    /// Provider-neutral completion reason, when the provider reports one.
    pub(crate) completion_reason: Option<CompletionReason>,
}

impl AssistantTurn {
    #[cfg(test)]
    pub(crate) fn text(text: &str) -> Self {
        Self {
            text: Some(text.to_string()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ModelOrigin {
    pub(crate) provider: String,
    pub(crate) api: String,
    pub(crate) model: String,
}

impl ModelOrigin {
    pub(crate) fn new(provider: &str, api: &str, model: &str) -> Self {
        Self {
            provider: provider.to_string(),
            api: api.to_string(),
            model: model.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReasoningBlock {
    pub(crate) text: String,
    pub(crate) continuity: Option<String>,
    pub(crate) redacted: bool,
    pub(crate) origin: ModelOrigin,
}

impl ReasoningBlock {
    pub(crate) fn new(
        text: &str,
        continuity: Option<&str>,
        redacted: bool,
        origin: ModelOrigin,
    ) -> Self {
        Self {
            text: text.to_string(),
            continuity: continuity.map(str::to_string),
            redacted,
            origin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: Value,
    // Opaque provider continuity token for this call (e.g. Gemini's
    // `thoughtSignature`). Echoed back verbatim in the next request's history so
    // thinking models accept the tool round-trip; `None` for providers that do
    // not emit one. Nexus never interprets it.
    pub(crate) thought_signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Message {
    pub(crate) role: Role,
    pub(crate) content: String,
    // Tool-call and tool-result messages must carry both fields; text messages leave them empty.
    pub(crate) tool_call_id: Option<String>,
    pub(crate) tool_name: Option<String>,
    // Provider-neutral reasoning continuity. Only AssistantReasoning rows use
    // these; Nexus stores them opaquely and never interprets signatures/data.
    pub(crate) continuity: Option<String>,
    // Nexus-owned provider/model round-trip id. Present on assistant/reasoning/
    // tool-call/tool-result messages produced by a provider turn; absent on
    // user messages and legacy resumed entries.
    pub(crate) provider_turn_id: Option<String>,
    pub(crate) redacted: bool,
    pub(crate) origin: Option<ModelOrigin>,
    /// Opaque provider-owned compaction envelopes attached only to synthetic
    /// summary messages. Adapters replay matching envelopes; every other lane
    /// ignores them and consumes `content` as the portable summary.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) provider_blocks: Vec<Value>,
}

/// One visible user prompt plus optional hidden provider-context messages.
/// Wayland constructs this at a turn boundary; Nexus preserves ordering and
/// persistence without interpreting the context payload.
pub(crate) struct TurnInput<'a> {
    prompt: &'a str,
    context: Vec<Message>,
}

impl<'a> TurnInput<'a> {
    pub(crate) fn new(prompt: &'a str) -> Self {
        Self {
            prompt,
            context: Vec::new(),
        }
    }

    pub(crate) fn with_context(prompt: &'a str, context: Vec<Message>) -> Self {
        Self { prompt, context }
    }
}

impl Message {
    pub(crate) fn user(content: &str) -> Self {
        Self::new(Role::User, content)
    }

    pub(crate) fn developer(content: &str) -> Self {
        Self::new(Role::Developer, content)
    }

    pub(crate) fn assistant(content: &str) -> Self {
        Self::new(Role::Assistant, content)
    }

    #[cfg(test)]
    pub(crate) fn assistant_reasoning(
        content: &str,
        continuity: &str,
        redacted: bool,
        origin: ModelOrigin,
    ) -> Self {
        Self::assistant_reasoning_block(ReasoningBlock::new(
            content,
            Some(continuity),
            redacted,
            origin,
        ))
    }

    pub(crate) fn assistant_reasoning_block(block: ReasoningBlock) -> Self {
        Self {
            role: Role::AssistantReasoning,
            content: block.text,
            tool_call_id: None,
            tool_name: None,
            continuity: block.continuity,
            provider_turn_id: None,
            redacted: block.redacted,
            origin: Some(block.origin),
            provider_blocks: Vec::new(),
        }
    }

    pub(crate) fn assistant_tool_call(call: &ToolCall) -> Self {
        Self {
            role: Role::AssistantToolCall,
            content: call.arguments.to_string(),
            tool_call_id: Some(call.id.clone()),
            tool_name: Some(call.name.clone()),
            // Carry the provider's opaque per-call continuity (e.g. Gemini
            // `thoughtSignature`) so it survives persistence and is echoed back.
            continuity: call.thought_signature.clone(),
            provider_turn_id: None,
            redacted: false,
            origin: None,
            provider_blocks: Vec::new(),
        }
    }

    pub(crate) fn tool_result(call_id: &str, name: &str, content: &str) -> Self {
        Self {
            role: Role::Tool,
            content: content.to_string(),
            tool_call_id: Some(call_id.to_string()),
            tool_name: Some(name.to_string()),
            continuity: None,
            provider_turn_id: None,
            redacted: false,
            origin: None,
            provider_blocks: Vec::new(),
        }
    }

    fn new(role: Role, content: &str) -> Self {
        Self {
            role,
            content: content.to_string(),
            tool_call_id: None,
            tool_name: None,
            continuity: None,
            provider_turn_id: None,
            redacted: false,
            origin: None,
            provider_blocks: Vec::new(),
        }
    }

    pub(crate) fn with_provider_blocks(mut self, provider_blocks: Vec<Value>) -> Self {
        self.provider_blocks = provider_blocks;
        self
    }

    pub(crate) fn with_provider_turn_id(mut self, provider_turn_id: &str) -> Self {
        self.provider_turn_id = Some(provider_turn_id.to_string());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Developer,
    User,
    Assistant,
    AssistantReasoning,
    AssistantToolCall,
    Tool,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Developer => "developer",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::AssistantReasoning => "assistant_reasoning",
            Self::AssistantToolCall => "assistant_tool_call",
            Self::Tool => "tool",
        }
    }

    /// Inverse of [`as_str`](Self::as_str): parse a persisted role string back
    /// into a `Role`. Used by the session store to reconstruct messages when
    /// reading a transcript. `None` for an unknown role.
    pub(crate) fn from_wire(role: &str) -> Option<Self> {
        match role {
            "developer" => Some(Self::Developer),
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "assistant_reasoning" => Some(Self::AssistantReasoning),
            "assistant_tool_call" => Some(Self::AssistantToolCall),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// Pair a trailing tool call that has no recorded result. A prior session that
/// crashed between persisting an `AssistantToolCall` and its `Tool` result
/// leaves the call unanswered as the last entry; appending a new user prompt
/// then yields a sequence providers reject (every tool call must be answered).
/// At most one such call can dangle (the loop records each call's result
/// adjacently), so one synthetic result restores validity. The appended message
/// is new (beyond the persisted cursor), so the harness writes it to the same
/// log, keeping disk and memory consistent.
fn next_provider_turn_seq(messages: &[Message]) -> u32 {
    messages
        .iter()
        .filter_map(|message| message.provider_turn_id.as_deref())
        .filter_map(|id| id.strip_prefix("turn_"))
        .filter_map(|seq| u32::from_str_radix(seq, 16).ok())
        .max()
        .map_or(0, |seq| seq.saturating_add(1))
}

fn repair_dangling_tool_call(messages: &mut Vec<Message>) {
    let mut pending: Vec<(String, String, Option<String>)> = Vec::new();
    for message in messages.iter() {
        match message.role {
            Role::AssistantToolCall => {
                if let (Some(call_id), Some(name)) = (&message.tool_call_id, &message.tool_name) {
                    pending.push((
                        call_id.clone(),
                        name.clone(),
                        message.provider_turn_id.clone(),
                    ));
                }
            }
            Role::Tool => {
                if let Some(call_id) = &message.tool_call_id
                    && let Some(pos) = pending.iter().position(|(id, _, _)| id == call_id)
                {
                    pending.remove(pos);
                }
            }
            Role::Developer | Role::User | Role::Assistant | Role::AssistantReasoning => {}
        }
    }
    for (call_id, name, provider_turn_id) in pending {
        let mut result = Message::tool_result(
            &call_id,
            &name,
            &ToolResultContract::cancelled().into_wire_json(),
        );
        result.provider_turn_id = provider_turn_id;
        messages.push(result);
    }
}

/// Build the model-facing JSON for a successful tool result, offloading the
/// content behind a handle when it is oversized and a store is available
/// (issue #61). Small outputs, and the fallback when no store exists or the
/// store write fails, are byte-identical to the original inline encoding -- the
/// full output is never truncated and discarded.
fn success_tool_result_json(
    store: Option<&dyn ToolOutputStore>,
    mut output: ToolOutput,
) -> (String, Option<OutputHandleMetadata>) {
    if output.content.len() <= MAX_INLINE_TOOL_OUTPUT_BYTES {
        return (ToolResultContract::success(output).into_wire_json(), None);
    }
    let Some(store) = store else {
        // No durable store (e.g. in-memory session): keep the full output inline
        // rather than lose it. Larger context, but never data loss.
        return (ToolResultContract::success(output).into_wire_json(), None);
    };
    match store.put(&output.content) {
        Ok(handle_id) => {
            // Swap the oversized content for a compact preview and record the
            // typed handle pointer in metadata, then serialize through the same
            // contract as an inline result -- one serialization path, so the
            // offloaded and inline shapes cannot drift apart.
            let total_bytes = output.content.len();
            let total_lines = output.content.lines().count();
            output.content = compact_preview(&output.content, &handle_id, total_bytes, total_lines);
            let info = OutputHandleMetadata {
                id: handle_id,
                bytes: total_bytes,
                lines: total_lines,
            };
            output
                .metadata
                .insert("outputHandle".to_string(), info.to_value());
            (
                ToolResultContract::success(output).into_wire_json(),
                Some(info),
            )
        }
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "tool output handle store failed; inlining full output"
            );
            (ToolResultContract::success(output).into_wire_json(), None)
        }
    }
}

// `compact_preview` is only reached for offloaded outputs, which by construction
// exceed MAX_INLINE_TOOL_OUTPUT_BYTES. This compile-time assert ties that to the
// preview sizing, so head and tail never overlap and `len - PREVIEW_TAIL_BYTES`
// never underflows -- tuning a constant that breaks the invariant fails the
// build instead of producing a malformed preview at runtime.
const _: () = assert!(MAX_INLINE_TOOL_OUTPUT_BYTES > PREVIEW_HEAD_BYTES + PREVIEW_TAIL_BYTES);

/// Head + tail of an oversized output with a middle-elision notice naming the
/// handle. Slices land on UTF-8 char boundaries via the stdlib boundary helpers;
/// head and tail are disjoint and the tail offset cannot underflow because an
/// offloaded output is strictly larger than `PREVIEW_HEAD_BYTES +
/// PREVIEW_TAIL_BYTES` (the const assert above).
fn compact_preview(
    content: &str,
    handle_id: &str,
    total_bytes: usize,
    total_lines: usize,
) -> String {
    let head = &content[..content.floor_char_boundary(PREVIEW_HEAD_BYTES)];
    let tail = &content[content.ceil_char_boundary(content.len() - PREVIEW_TAIL_BYTES)..];
    let omitted = total_bytes.saturating_sub(head.len() + tail.len());
    let notice = format!(
        "\n... [iris stored the full {total_bytes}-byte ({total_lines}-line) tool output out of \
         context; {omitted} bytes omitted here. retrieve via output handle {handle_id}] ...\n"
    );
    format!("{head}{notice}{tail}")
}

#[cfg(test)]
#[path = "nexus_tests.rs"]
mod tests;

// End-to-end tokens-per-completed-task benchmark harness (issue #210). Sibling
// test module (crate-private access to the Nexus loop, ApprovalMode, and the
// tool env) driving the deterministic replay arms and the opt-in real-provider
// headline. See `docs/BENCHMARK_PLAN.md`.
#[cfg(test)]
#[path = "bench_tokens_per_task.rs"]
mod bench_tokens_per_task;

// Compaction retention-needle benchmark scaffold (ADR-0045, issue #372). Lives
// beside the Nexus test module so it can reuse the in-crate provider/message
// types via `use super::*`, while driving the Tier-2 `wayland` compaction seam.
#[cfg(test)]
#[path = "compaction_bench.rs"]
mod compaction_bench;

// Env-gated LIVE anchor for the modeled cache economics (ADR-0045, #372). Kept
// beside `compaction_bench` so it reuses the in-crate provider/message types via
// `use super::*`. Double-gated (`#[ignore]` + `IRIS_BENCH_LIVE=1`), so the gate's
// `cargo test` never issues a live API call.
#[cfg(test)]
#[path = "compaction_live_bench.rs"]
mod compaction_live_bench;

// Compaction live-measurement CAMPAIGN harness (design:
// compaction-live-harness). Generalizes the per-experiment live bench above
// into a lane x scenario x settings x n matrix runner with a uniform row
// schema and resumable manifests. Sibling test module so it reuses the in-crate
// provider/message types via `use super::*`; double-gated (`#[ignore]` +
// `IRIS_BENCH_LIVE=1`) so the gate's `cargo test` never issues a live call.
#[cfg(test)]
#[path = "live_harness/mod.rs"]
mod live_harness;
