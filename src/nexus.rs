use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Error, Result, bail};
use futures::Stream;
use futures::StreamExt;
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

    /// Resolve the startup approval posture from the persisted `defaultApproval`
    /// setting (GLOBAL-ONLY): a valid token is applied, while an absent or
    /// invalid value leaves today's default (`strict`) so a missing or typo'd
    /// setting never changes posture. The live `/approval` command is
    /// unaffected and stays session-only.
    pub(crate) fn from_startup_setting(setting: Option<&str>) -> Self {
        setting.and_then(Self::parse).unwrap_or_default()
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

/// The semantic events the loop emits during a turn. Provider- and UI-neutral:
/// a front-end maps these onto its own rendering. Mirrors pi's `AgentEvent`
/// union (`packages/agent/src/types.ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentEvent {
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
        budget: u64,
        /// 1-based compaction generation ordinal (ADR-0047): the Nth compaction
        /// in the session reports N. Instrumentation of compaction depth; does
        /// not affect range selection or summary content.
        generation: u64,
        /// Number of workspace-relative touched/read paths carried verbatim
        /// alongside the prose summary (ADR-0044). Additive instrumentation; 0
        /// when the covered range had no in-workspace tool targets.
        carried_paths: usize,
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
    ToolProposed(ToolCall),
    /// A tool is about to execute (emitted once per call, immediately before the
    /// run, on both the exclusive and parallel paths). Lets a front-end open a
    /// live progress cell before any output arrives. Display-only.
    ToolStarted(ToolCall),
    /// A gated tool was auto-approved by the session allow-policy or the
    /// persistent project policy (ADR-0027). Emitted by Nexus, never inferred
    /// by a front-end, so the policy stays Nexus-owned.
    ToolAutoApproved(ToolCall),
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
    /// Terminal event: the fully assembled assistant turn.
    Completed(AssistantTurn),
}

/// A `!Send` boxed stream of provider events tied to the borrow of the provider
/// and its inputs. Boxed (not `impl Stream`) so the loop code is uniform and the
/// real provider can back it with a channel fed by a blocking task.
pub(crate) type ProviderStream<'a> = Pin<Box<dyn Stream<Item = Result<ProviderEvent>> + 'a>>;

/// A `!Send` boxed tool-execution future, so `Box<dyn Tool>` stays object-safe
/// while `execute` is async.
pub(crate) type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput>> + 'a>>;

/// A `!Send` boxed approval future, so `&dyn ApprovalGate` stays object-safe
/// while `review` is async (and therefore raceable against cancellation).
pub(crate) type ApprovalFuture<'a> = Pin<Box<dyn Future<Output = Result<ApprovalDecision>> + 'a>>;

/// Fire-and-forget event sink the loop emits to. `&self` with no control-flow
/// return; errors only propagate. Mirrors pi's standalone `AgentEventSink`
/// passed as a separate argument, not a config field.
pub(crate) trait AgentObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()>;
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
    /// gate fired, so "always" here means "all dirty files this task" and no
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

    /// Snapshot the protected set's contents before a mutating call executes so
    /// an out-of-band write can be detected and restored afterward. `paths` are
    /// the call's statically-known mutation targets (empty for `bash`): the
    /// guard also snapshots their pre-call bytes so the checkpoint chain (#263)
    /// can capture the exact pre-task content of a clean file Iris is about to
    /// edit, not just the pre-existing dirty set.
    fn before_exec(&self, paths: &[PathBuf]);

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
    fn after_exec(&self, approved: &[PathBuf], expected_after: Option<&str>) -> Vec<PathBuf>;

    /// Restore the given protected files from the pre-exec snapshot. Best-effort
    /// recovery invoked by the loop after a detected violation.
    fn restore(&self, paths: &[PathBuf]) -> Result<()>;
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
}

/// Result of consuming one provider stream to its terminal event (or to a
/// cancellation). Owned so the borrow of `self.messages`/`self.tools` taken by
/// the stream is released before the loop mutates the transcript.
enum StreamResult {
    Completed {
        turn: AssistantTurn,
        saw_delta: bool,
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
            next_provider_turn_seq: 0,
            max_tool_roundtrips: None,
            mutated_this_turn: false,
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
            next_provider_turn_seq,
            max_tool_roundtrips: None,
            mutated_this_turn: false,
        }
    }

    /// Read access to the in-memory transcript so the harness can persist it
    /// without the core loop owning a session store.
    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
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
        // Reset the per-turn mutation signal so the harness's post-change
        // verification trigger (issue #265) reflects only this turn: a pure Q&A
        // turn stays `false` and runs no verification. A retry turn resets it too,
        // so "did the model make further changes" is measured fresh each attempt.
        self.mutated_this_turn = false;
        // Index of the just-pushed prompt: the start of the unanswered user run
        // a cancellation before any provider answer truncates back to.
        let unanswered_start = self.messages.len();
        self.messages.push(Message::user(prompt));
        // The bare agent does no persistence: the harness diffs `messages()`
        // onto its session store after the turn returns (even on error).
        self.complete_turn(unanswered_start, obs, gate, env, token, steer)
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
            ToolOutcome::Denied => VerifyRun::Denied,
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
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
        steer: Option<&dyn SteeringSource>,
    ) -> Result<()> {
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

            let stream_result = match self.stream_turn(obs, token).await {
                Ok(result) => result,
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
                StreamResult::Completed { turn, saw_delta } => {
                    let AssistantTurn {
                        text,
                        reasoning,
                        tool_calls,
                        response_id,
                        usage,
                        completion_reason,
                    } = turn;
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
                        if block.redacted {
                            obs.on_event(AgentEvent::AssistantReasoning {
                                text: String::new(),
                                redacted: true,
                            })?;
                        } else if !block.text.is_empty() {
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

                    obs.on_event(AgentEvent::ProviderTurnCompleted {
                        turn_id: provider_turn_id.clone(),
                        response_id,
                        usage,
                        completion_reason,
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

                    match self
                        .run_tools(tool_calls, obs, gate, env, token, &provider_turn_id)
                        .await?
                    {
                        ToolsPhase::Ended => return Ok(()),
                        ToolsPhase::Continue => {}
                    }
                }
            }
            roundtrip += 1;
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
                        partial.push_str(&delta);
                        obs.on_event(AgentEvent::AssistantTextDelta(delta))?;
                    }
                    Some(Ok(ProviderEvent::Activity)) => {}
                    Some(Ok(ProviderEvent::Completed(turn))) => {
                        return Ok(StreamResult::Completed { turn, saw_delta });
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
        self.tools
            .by_name(&call.name)
            .is_some_and(|tool| tool.is_concurrency_safe() && !tool.requires_approval())
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
            .map(|path| {
                path.strip_prefix(env.workspace)
                    .unwrap_or(path)
                    .display()
                    .to_string()
            })
            .collect();
        if let Some(tool) = self.tools.by_name(&call.name) {
            if let Some(diff) = tool.diff_preview(env.workspace, &call.arguments) {
                obs.on_event(AgentEvent::DiffPreview {
                    call: call.clone(),
                    diff,
                })?;
            }
            if !tool.requires_approval() && !dirty_gate {
                obs.on_event(AgentEvent::ToolProposed(call.clone()))?;
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
                        // dirty-tree gate offers the "all dirty files this task"
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
                // Announce execution start (so a front-end opens a live cell)
                // only for a real tool, then inject a per-call streaming emitter
                // for the run. An unknown tool never opens a phantom cell and
                // keeps an exact ToolStarted/ToolResult pairing. Only this
                // exclusive path streams (the parallel/exploration path stays
                // sink-less), so a single live exec cell is always enough.
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
                // Dirty-tree detection layer (issue #262, ADR-0028): snapshot the
                // protected set before a mutating call, then re-check it after.
                // A protected file changed out-of-band (not an approved target of
                // this call) is a violation: fail the call, restore from
                // snapshot, and surface which files changed.
                let guard = env.mutation_guard.filter(|_| is_mutating);
                if let Some(guard) = guard {
                    guard.before_exec(&mutated_paths);
                }
                let outcome = run_tool(tool, &call.arguments, &call_env, token.child_token()).await;
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
                    let violations = guard.after_exec(&mutated_paths, expected_after.as_deref());
                    if !violations.is_empty() {
                        let restored = guard.restore(&violations).is_ok();
                        let paths: Vec<String> = violations
                            .iter()
                            .map(|path| {
                                path.strip_prefix(env.workspace)
                                    .unwrap_or(path)
                                    .display()
                                    .to_string()
                            })
                            .collect();
                        obs.on_event(AgentEvent::MutationViolation {
                            call: call.clone(),
                            paths: paths.clone(),
                            restored,
                        })?;
                        let recovery = if restored {
                            "; restored from snapshot"
                        } else {
                            "; snapshot restore failed"
                        };
                        ToolOutcome::Err(anyhow::anyhow!(
                            "halted: modified protected uncommitted file(s): {}{recovery}",
                            paths.join(", ")
                        ))
                    } else {
                        outcome
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
    Denied,
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
        Self::Denied
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
            Self::Denied => json!({
                "ok": false,
                "error": "tool call denied by user",
                "denied": true
            }),
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
    };
    obs.on_event(event)
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Per-retention breakdown of prompt-cache-creation (write) input tokens, as
/// reported by Anthropic's `usage.cache_creation` detail. Surfaced alongside
/// the `cache_write_input_tokens` total so diagnostics can attribute writes to
/// the 5-minute vs 1-hour cache tier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl Message {
    pub(crate) fn user(content: &str) -> Self {
        Self::new(Role::User, content)
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
        }
    }

    pub(crate) fn with_provider_turn_id(mut self, provider_turn_id: &str) -> Self {
        self.provider_turn_id = Some(provider_turn_id.to_string());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
    AssistantReasoning,
    AssistantToolCall,
    Tool,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
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
            Role::User | Role::Assistant | Role::AssistantReasoning => {}
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

// Compaction retention-needle benchmark scaffold (ADR-0045, issue #372). Lives
// beside the Nexus test module so it can reuse the in-crate provider/message
// types via `use super::*`, while driving the Tier-2 `wayland` compaction seam.
#[cfg(test)]
#[path = "compaction_bench.rs"]
mod compaction_bench;
