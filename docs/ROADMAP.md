# Iris — Roadmap

> Status (2026-06-17): Milestone 1 is complete. Iris has a text-only session
> loop, OpenAI Codex Responses provider, streamed response parsing, workspace-
> scoped tools, terminal approval gates with diff previews, provider/model
> settings, and best-effort JSONL transcript persistence. The next runtime work
> is to finish Nexus's async-hard agent loop before token/context milestones.
> This roadmap defines build order and acceptance criteria. `FEATURES.md` remains
> the capability inventory; this document says what to build first.

This is not an implementation plan. It should define sequencing, scope boundaries,
quality gates, and open design decisions. Detailed module structure, Rust types,
tool schemas, provider payloads, and UI flows belong in later implementation notes
or design specs.

## Product direction

Iris is an end-user terminal coding agent. The long-term thesis is token-deliberate
coding: every model call should be budgeted, justified, cached where possible, and
connected to the diff the user ships.

## Product terminology

- **Iris** — the coding agent and overall product.
- **Nexus** — the agent runtime core inside Iris. Nexus runs the model loop,
  tool execution, context handling, safety checks, and later modes/subagents.
- **Iris CLI** — the terminal interface users run to interact with Iris.

For the first MVP, Iris CLI and Nexus will be built together because the terminal
is the only interface. Over time, the distinction matters: Nexus is the engine;
Iris CLI is one interface; Iris is the product experience.

Roadmap-level boundary:

- Iris CLI should own terminal interaction.
- Nexus should own the model loop, tool execution protocol, conversation state,
  safety policy enforcement, and later context/mode/subagent behavior.
- Provider and tool details should sit behind explicit seams so later milestones do
  not require rewriting the first loop.

The immediate post-Milestone-1 goal is to finish the agent runtime loop before
building token/context systems on top of it.

## Current implementation snapshot

Implemented today:

- CLI entrypoint that starts Iris from `cargo run`.
- Text-only Nexus session loop with in-memory conversation state, `/exit` /
  `/quit`, and provider-error recovery, driven through a `Ui` front-end seam
  (`src/ui/`, `src/cli.rs`).
- Incremental terminal streaming of assistant text via the `TurnSink` seam and
  `UiEvent` deltas.
- Provider-neutral `ChatProvider`, `TurnSink`, `AssistantTurn`, `ToolCall`,
  `Message`, and `Role` types.
- Provider tool-call loop with bounded iterations, retry/backoff, and structured
  tool result/error messages.
- Typed boundary errors with process exit codes (`src/errors.rs`) and `RUST_LOG`
  tracing to stderr (`src/telemetry.rs`).
- Workspace-scoped built-in tools: `read`, `write`, `edit`, `bash`, `grep`,
  `find`, and `ls`. `edit` follows Claude Code's exact-string contract
  (`file_path`/`old_string`/`new_string`/`replace_all`).
- Workspace path-safety enforcement for existing and newly written paths.
- Terminal approval prompts with diff previews for file-mutating tools, and
  denied-call handling for `write`, `edit`, and `bash`.
- Atomic same-directory file replacement helper used by `write` and `edit`.
- Text-only `read` rejects binary/NUL-containing and invalid UTF-8 files instead
  of rendering lossy text.
- OpenAI Codex OAuth token loading/refresh from the Iris auth-file shape.
- OpenAI Codex browser and device-code login flows.
- OpenAI Codex Responses request/response handling, including tool schemas and
  streamed-response parsing.
- Unit tests for the REPL, tool loop, approvals, tool implementations, path
  safety, atomic writes, auth-file handling, URL/request shaping, and response
  parsing.

Not implemented yet:

- Runtime-hard async provider/tool contracts, turn-level cancellation tokens,
  stream/tool cancellation races, child cancellation per tool, and safe parallel
  tool execution.
- Persistent approval policies, session `/resume` and transcript-tree branching,
  modes, subagents, context ledger, content handles, git automation, and GitHub
  integration.

## Runtime completion — finish Nexus before Milestone 2

**Goal:** finish the agent runtime, not just the feature checklist. Nexus should
keep pi-mono's clean contracts-in/events-out shape while adopting the mature
Rust async mechanics used by Codex CLI.

This work is the next sequencing gate before Milestone 2. The current loop is
clear and already parses provider streams, but the provider/tool seams are still
too blocking for a terminal agent: cancellation is observed between steps rather
than raced against in-flight provider reads and long-running tools.

### Reference split

- **Use `~/vendor/pi-mono` for shape.** pi-mono is already async TypeScript, but
  deliberately linear: `packages/agent/src/agent-loop.ts` shows the simple
  `runLoop`, `streamAssistantResponse`, `beforeToolCall`, `prepareNextTurn`,
  `transformContext`, and `convertToLlm` seams. Keep this clarity.
- **Use `~/vendor/codex` for Rust runtime mechanics.** Codex CLI is the primary
  Rust reference for Tokio streams, `CancellationToken`, `tokio::select!` /
  cancellation races, bounded channels, child cancellation per tool, and
  safe-parallel/exclusive tool execution. Start with
  `codex-rs/core/src/client.rs`, `codex-rs/core/src/session/turn.rs`,
  `codex-rs/core/src/tools/parallel.rs`, and
  `codex-rs/core/src/stream_events_utils.rs`.
- **Use `~/vendor/claude-code` only for product edge cases.** It validates the
  same pattern with `AbortController`, async generators, synthetic cancelled tool
  results, and concurrency-safe batching. It is not a Rust architecture source.
- **Use `~/vendor/pi_agent_rust` only as a secondary sketch.** Do not adopt
  `asupersync`, a bespoke runtime, or a monolithic `agent.rs` structure.

### Required runtime behavior

1. `ChatProvider` becomes async streaming, yielding typed provider-neutral events
   instead of one blocking whole-turn result.
2. Each submitted turn owns a real cancellation token. Ctrl-C or equivalent
   cancellation cancels that token.
3. The turn loop races provider stream reads against cancellation.
4. `Tool::execute` is async and receives a child cancellation token.
5. Tool execution races the tool future against cancellation.
6. Tool calls are sequential by default.
7. Only tools explicitly marked concurrency-safe/read-only may overlap; unsafe
   tools run exclusively and preserve transcript order.
8. Provider/tool errors and cancellations are represented as turn/tool data when
   possible, not panics.
9. If cancellation happens after the model has emitted tool calls, the transcript
   remains valid by recording either real tool results or synthetic cancelled
   tool-result data for every emitted call.

### Acceptance criteria

Runtime completion is done only when tests prove:

1. A fake streaming provider emits multiple events in order and the observer sees
   them in order.
2. Cancelling while a fake provider stream is pending exits promptly and leaves
   valid turn state.
3. Async tools are awaited and their results feed a follow-up model turn.
4. Cancelling while a fake tool is pending exits promptly and records a valid
   cancelled/error tool result.
5. Two unsafe tools do not overlap.
6. Two explicitly safe tools do overlap, while their recorded results remain
   deterministic.
7. Safe tools do not cross an unsafe tool in a way that changes effects or
   transcript order.
8. Pending approval, if any, unblocks on cancellation.
9. Existing observer, approval, provider, tool, path-safety, and transcript tests
   remain green.

Verification gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
and `cargo test`. If any check cannot run or fails for a pre-existing reason,
the implementation report must include the exact command output and the smallest
follow-up needed.

### Explicit non-goals

- No custom runtime.
- No `asupersync`.
- No giant all-purpose `agent.rs`.
- No Codex WebSocket reuse or transport fallback machinery unless a real Iris
  provider bug requires it.
- No Claude Code full streaming-tool executor unless the simple safe/unsafe
  batching rule is proven insufficient.
- No context compaction, subagents, provider routing, or steering UX before the
  async runtime seam exists.

## Tool quality — best-in-class on all tools

Cross-cutting workstream, not a milestone. Goal: every built-in tool is at or
above the field's best implementation. The seven tools were assessed 2026-06-15
against Claude Code, Codex CLI, Aider, OpenHands/SWE-agent, Cline/Roo, Gemini
CLI, and oh-my-pi (omp). Each tool has a tracking issue.

| Tool | Tier today | Best-in-class holder | Issue |
| --- | --- | --- | --- |
| `edit` | Claude-compatible exact-string + replace_all + atomic writes | RooCode (fuzzy) / Codex (V4A) | [#4](https://github.com/5omeOtherGuy/iris-agent/issues/4) |
| `grep` | Native: ripgrep library, no `rg` binary | Claude Code / omp | [#6](https://github.com/5omeOtherGuy/iris-agent/issues/6) |
| `write` | Standard + atomic writes | Claude Code | [#5](https://github.com/5omeOtherGuy/iris-agent/issues/5) |
| `ls` | Standard | commoditized | [#8](https://github.com/5omeOtherGuy/iris-agent/issues/8) |
| `read` | Standard text read | Claude Code (multimodal; deferred) | [#2](https://github.com/5omeOtherGuy/iris-agent/issues/2) |
| `find` | Native: `ignore` + `globset`, no `fd` binary | Claude Code Glob (native) | [#7](https://github.com/5omeOtherGuy/iris-agent/issues/7) |
| `bash` | Hardened: kernel sandbox + persistent sessions + background jobs + force-quit reaping | Claude Code / Codex | [#3](https://github.com/5omeOtherGuy/iris-agent/issues/3) |

Execution order (by impact/effort, independent of the milestone sequence):

1. **Tier 1 — keep the honesty fixes shipped.** The earlier false-advertising
   bug is fixed in code: `read` no longer renders invalid UTF-8 as lossy text.
   Keep docs/issues aligned as behavior changes —
   [#2](https://github.com/5omeOtherGuy/iris-agent/issues/2).
   The content-hash anchored `hashline_edit` tool and the `read`/`grep`
   `hashline` option were removed in favor of a single Claude-compatible
   exact-string `edit` path; re-add only if exact-string edits prove
   unreliable in real use.
2. **Tier 2 — close real capability gaps.**
   - `bash`: **shipped** as four committed subsystems —
     [#3](https://github.com/5omeOtherGuy/iris-agent/issues/3):
     1. Kernel sandbox (`src/tools/bash/sandbox.rs`): Landlock LSM confines
        every shell to workspace + temp-dir writes with TCP deny-by-default;
        ruleset built in the parent, enforced in an async-signal-safe
        `pre_exec`; fail-open is never silent (notice + `tracing::warn`).
        macOS Seatbelt is not implemented — Linux-only enforcement today;
        unsupported kernels run unconfined with a surfaced notice.
     2. Persistent sessions (`src/tools/bash/session.rs`): opt-in `session` id;
        `cd`/env/shell vars persist via a co-process + sentinel-marker
        protocol; lazy create, explicit `reset`/`close`, Drop closes all.
     3. Background jobs (`src/tools/bash/jobs.rs`): `action=start/poll/`
        `finalize/cancel/list`; bounded byte-ring per job with dropped-byte
        accounting; one worker thread drains then reaps (condvar, no
        busy-wait); finished-job map bounded.
     4. Process-group reaping (`src/process_group.rs`): centralized
        spawn/kill/registry primitive; the second-Ctrl-C force-quit
        (#9) now SIGKILLs every tracked child group (async-signal-safe) so a
        long-running shell, session, or job is no longer orphaned.
     ponytail / known ceilings: TCP-only network confinement (UDP/UNIX
     sockets need a network namespace); a `setsid`/double-fork child can
     still escape the group; finalized job output is lossy UTF-8 at
     poll/ring boundaries; Landlock fail-closed (`Required`) mode is the
     documented upgrade path.
   - `find`/`grep`: **shipped native** (PR #19). `grep` searches through the
     ripgrep library and `find` walks via `ignore` + `globset`, dropping the
     `rg`/`fd` binary dependencies and the missing-binary failure mode. This
     reverses the earlier "keep wrapping `fd`/`rg`" call: going native removed
     the runtime-dependency/packaging problem outright rather than taking on a
     parity burden, and reuses the same upstream engines as libraries —
     [#7](https://github.com/5omeOtherGuy/iris-agent/issues/7),
     [#6](https://github.com/5omeOtherGuy/iris-agent/issues/6).
3. **Tier 3 — parity polish.** `edit` `replace_all` + helpful failure output
   ([#4](https://github.com/5omeOtherGuy/iris-agent/issues/4)) — shipped; `write`
   freshness guard ([#5](https://github.com/5omeOtherGuy/iris-agent/issues/5)) —
   shipped (read-before-mutate via the session observation store, shared with
   `edit`); `ls` metadata
   ([#8](https://github.com/5omeOtherGuy/iris-agent/issues/8)) — shipped (opt-in
   `long` mode: type marker + human-readable size).
   `read` multimodal support for PDF/notebook/image inputs
   ([#2](https://github.com/5omeOtherGuy/iris-agent/issues/2)) is explicitly
   deferred as a nice-to-have for much later.
4. **Tier 4 — extend the frontier.** New tools beyond the seven —
   `apply_patch` (V4A; tracked under provider-specific tools,
   [#10](https://github.com/5omeOtherGuy/iris-agent/issues/10)), `ast_edit` for
   structural moves, and an optional fast-apply path.

Shared tool infrastructure issues opened 2026-06-15:

| Issue | Area | Current status |
| --- | --- | --- |
| [#11](https://github.com/5omeOtherGuy/iris-agent/issues/11) | Path identity and file observation store | Done (MVP): session-scoped `ObservedFiles` records `{mtime, content_hash}` per canonical path on read/write/edit (`src/tools/observe.rs`). |
| [#12](https://github.com/5omeOtherGuy/iris-agent/issues/12) | Mutation preflight and stale-file detection | Done (MVP): `edit`/`write` reject mutating an existing file that was never read or changed since last read (hash-decided; mtime refreshed on benign change). New files may be created blind. |
| [#13](https://github.com/5omeOtherGuy/iris-agent/issues/13) | Atomic file mutation layer | Partial: same-directory atomic replacement helper exists; observation refresh now happens after each mutation; no canonical mutation queue. |
| [#14](https://github.com/5omeOtherGuy/iris-agent/issues/14) | Diff/preview and approval policy | Done (MVP): Nexus enforces a session allow-policy. Approval offers `[y] once` / `[a] always this session` / `[N] deny`; `always` records the tool name in a Nexus-owned `session_allowed` set so later same-tool calls auto-approve (emitted as `ToolAutoApproved`, never inferred by the UI). Deny stays safe-by-default (empty/invalid/EOF). Diff previews now render colored +/- with relative headers (the `a//abs` double-slash and write-vs-edit path inconsistency are fixed). Remaining: cross-session persistence, risk labels, and per-exact-command bash granularity (`always` on `bash` currently authorizes any later shell command this session). |
| [#15](https://github.com/5omeOtherGuy/iris-agent/issues/15) | Tool output/result/error contract | Done (MVP): `dispatch` returns a `ToolOutput { content, metadata }`; success results carry a per-tool `metadata` object on the wire (`read` byte/line/`truncated`, `ls` entries, `write` bytes, `edit` occurrences). Handle-backing for large outputs is the remaining Milestone 2 work. |

Status: strong-standard on the read/grep/edit/write/ls cluster, with `edit` now
on Claude Code's exact-string contract, a shared read-before-mutate stale-file
guard, and a structured `ToolOutput` result/metadata contract. The honest gaps
are `bash` (large), handle-backed large outputs (medium), persistent approval policy/risk
labels (medium; diff preview already shipped). The `rg`/`fd` packaging gap is
closed: `grep`/`find` are now native libraries with no external binary.

## Provider-specific tools

Complementary long-term axis to the tool-quality work above, tracked as an epic
([#10](https://github.com/5omeOtherGuy/iris-agent/issues/10)). The seven tools
stay provider-agnostic and remain the canonical baseline; *in addition*, when Iris
routes a turn to a given model it should present that model the tool surface it
was trained on, wherever a benchmark shows a win. Examples: OpenAI/Codex
`apply_patch` (V4A), Anthropic native `text_editor`/`bash` tool types, a Gemini
`diff-fenced` edit variant.

Done incrementally — one provider-specific tool at a time, each behind a measured
advantage and a generic fallback, never as the only path. `apply_patch` (V4A) for
Codex routes is the first/reference instance. Requires a tool registry that can
vary the advertised tool set by active provider/model (plugs into the Milestone 4
routing work); result shape, path safety, and approval gates stay centralized in
Nexus regardless of which variant runs.

## Milestone 0 — Agent Kernel MVP

**Goal:** a developer can run Iris in a terminal, ask it to inspect or modify a
local project, approve/observe tool calls, and continue the conversation in a loop.

This is the minimum viable coding agent. It does not prove the full Iris token
thesis yet, but it creates the foundation required to prove it.

### Included

- **CLI entrypoint**
  - Start Iris from the terminal.
  - Accept user prompts interactively.
  - Print assistant responses. Streaming may be deferred to Milestone 1.
  - Exit cleanly.

- **Agent loop**
  - Maintain conversation state for the current session.
  - Send messages and tool schemas to one provider/model.
  - Receive assistant responses and tool calls.
  - Execute tool calls.
  - Feed tool results back into the model.
  - Continue until the assistant returns a final answer.

- **Provider support**
  - One working provider is enough for MVP.
  - Provider configuration may be simple: environment variable API key plus model
    name in config or CLI flags.
  - Multi-provider abstraction can be minimal, but should not block adding a second
    provider later.

- **Core tools**
  - `read`: read a text file from the workspace.
  - `write`: create or overwrite a file in the workspace.
  - `edit`: perform targeted text replacement in an existing file.
  - `bash`: run a shell command in the workspace.

- **Basic safety**
  - Restrict file tools to the current workspace by default.
  - Show tool calls and results clearly.
  - Require confirmation before `write`, `edit`, and `bash`
    (every mutating file/shell tool), unless explicitly run in an
    unsafe/non-interactive mode later.
  - Do not run destructive git operations automatically.
  - Do not commit, push, reset, rebase, or delete files without explicit user
    instruction.

- **Basic error handling**
  - Tool errors are returned to the model as tool results.
  - Provider errors are shown clearly.
  - Invalid tool calls fail safely.
  - The process exits cleanly on interrupt.

### Excluded from MVP

These are important, but not required for the first working agent:

- Context ledger.
- Content-addressed store.
- Handle-returning tool outputs.
- Cache-aware prompt layout.
- Prompt segment caching.
- Compaction.
- Modes.
- Subagents.
- Background workers.
- Tree-sitter repo map.
- Git checkpoint/rollback.
- GitHub integration.
- CI iteration.
- Anchored edits.
- Multi-provider matrix.
- SDK / embedding surface.

### Acceptance criteria

The Agent Kernel MVP is done when Iris can:

1. Start from the command line in a local repo. [Implemented]
2. Complete a multi-turn conversation with one model provider. [Implemented]
3. Read an existing file through the `read` tool. [Implemented]
4. Create a new file through the `write` tool after confirmation. [Implemented]
5. Modify an existing file through the `edit` tool after confirmation.
   [Implemented]
6. Run a harmless shell command through the `bash` tool after confirmation.
   [Implemented]
7. Return tool errors to the model without crashing. [Implemented]
8. Keep all file operations inside the workspace by default. [Implemented]
9. Exit cleanly without corrupting session state or files. [Implemented]
   `/exit`/`/quit` and EOF (Ctrl-D) end the loop cleanly; a graceful SIGINT
   handler turns the first Ctrl-C into a between-round-trips turn interrupt and
   lets a second Ctrl-C force-quit. Atomic writes guarantee no partial files.

### Remaining MVP design decisions

These should be specified in focused implementation notes rather than expanded
here:

- Whether destructive or externally visible `bash` commands need classification
  beyond the implemented baseline approval gate.
- Whether additional file-tool policy is needed for binary files, very large
  files, and absolute paths before marking path safety complete.

### MVP verification gate

Before Milestone 0 is considered complete, verification should include:

- Unit coverage for workspace path safety and edit behavior.
- Unit coverage for tool result/error encoding.
- Unit coverage for approval allow/deny paths.
- A fake-provider integration test covering prompt → tool call → tool result →
  final assistant response.
- A manual smoke test with one real provider. [Done 2026-06-15] Verified against
  the OpenAI Codex Responses provider: (1) `read` tool — prompt → tool call →
  result → final answer → clean exit; (2) `write` tool, approval allowed — diff
  preview → `y` → executed → result → final answer; (3) `write` tool, approval
  denied — diff preview → `n` → denied-call result returned to the model → no
  file created. All runs exited cleanly with status 0.

## Milestone 1 — Usable Local Coding Agent

**Goal:** make the kernel pleasant and safe enough for routine local coding tasks.

Potential scope:

- Better terminal UX for tool approvals and results. [Shipped: streamed-text
  presentation pass in `src/ui/text.rs` + `src/tool_display.rs` — visual
  hierarchy via per-block glyphs/color/spacing, colored unified diffs with
  fixed relative headers and minimized context, long tool results folded to a
  bounded preview with a `(+N more lines)` indicator, a session always-allow
  approval policy for file tools enforced in Nexus (`bash` still prompts every
  time), and paste-safe multi-line prompt input
  (bracketed paste + `\` continuation). All color/structure degrades to plain
  ANSI-free text on non-TTY/piped output. Deferred to a future full-TUI
  milestone (raw mode): interactive expand/collapse of folded blocks, `Alt+Enter`
  newline editing, and full right-bordered box framing.]
- Streaming output if not already in MVP. [Shipped: `TurnSink` deltas.]
- Session transcript persistence. [Shipped (linear transcript): `src/session.rs`
  writes a JSONL transcript -- a `session` header line plus one `message` line
  per entry -- to `<root>/<cwd-slug>/<unix-ms>_<id>.jsonl`, where `<root>` is
  `IRIS_SESSION_DIR` or `~/.iris/sessions`. The `Agent` appends new messages
  after each turn (best-effort: a write failure warns, never crashes the
  session; flushed per line so a crash leaves a valid prefix). Mirrors pi's
  session format at the MVP level. Deferred (later milestones): pi's tree/
  branch structure, compaction entries, labels, and `/resume` loading -- this
  ships write-only persistence, not session resume.]
- Focused config file for provider/model/tool policy. [Shipped (provider/model):
  `src/config.rs` loads JSON settings from `~/.iris/settings.json` (global,
  override via `IRIS_CONFIG_PATH`) and `<cwd>/.iris/settings.json` (project),
  project overriding global field-by-field, mirroring pi's settings model.
  Fields: `defaultProvider` (validated; only `openai-codex` today),
  `defaultModel`, `baseUrl`. Precedence is `env > settings > built-in default`
  so existing `IRIS_MODEL`/`IRIS_CODEX_BASE_URL` env vars still win; unknown
  keys are ignored, a malformed file errors with its path. Tool/approval policy
  is deliberately out of scope: pi's settings encode none either, and
  cross-session approval persistence is tracked under
  [#14](https://github.com/5omeOtherGuy/iris-agent/issues/14).]
- Safer `bash` policy. Command classification is optional and should not block
  the basic local coding workflow. [Shipped, stronger than classification:
  a Linux Landlock kernel sandbox confines every shell to workspace-write +
  TCP-deny with an explicit non-silent fallback, plus per-group spawn/kill/reap
  and force-quit reaping (`src/tools/bash/sandbox.rs`, `src/process_group.rs`,
  [#3](https://github.com/5omeOtherGuy/iris-agent/issues/3)). `bash` always
  prompts; blanket shell-command approval is deliberately disabled.]
- Better `edit` semantics: uniqueness checks, conflict messages, preview diff.
  [Shipped, at pi parity: exact match first, then a whitespace/Unicode-normalized
  fuzzy fallback (`normalize_for_fuzzy`/`locate_matches`, `src/tools/edit.rs`).
  Default match must be unique; ambiguous matches error with an occurrence count
  and suggest `replace_all`/more context, and not-found errors tell the model to
  re-read. This mirrors pi's `edit-diff.ts` strategy and its two error classes.
  Preview diff shipped for mutating tools. Deferred (nice-to-have): Aider-style
  "did you mean" near-match hints -- no source precedent in pi, and the
  misleading-suggestion/noise risk is not justified by the match-pi baseline.]
  See the Tool quality workstream ([#4](https://github.com/5omeOtherGuy/iris-agent/issues/4)).
- Basic git diff display after file changes. [Decided 2026-06-15: keep the
  pre-apply approval preview diff as the single diff surface, matching Claude
  Code and Codex CLI (verified: Codex generates one self-computed unified diff
  shown at approval time and never runs `git diff HEAD` for tool output). A
  prototype post-change `git diff HEAD` display was implemented and reverted: a
  brand-new file is untracked, so `git diff HEAD` shows nothing while the
  preview already renders it as full additions -- so for new files the git diff
  is invisible, and for tracked edits it just double-renders the preview. The
  apply-first, git-as-record model (Aider) is the genuinely better post-change
  surface, but it is only safe with checkpoint/undo rollback, so it moves to
  Milestone 5 alongside that work.]

Cut from Milestone 1 (2026-06-15): "Optional self-review before final
response." It collapsed two features of different sizes, neither M1 core. The
cheap version (one extra model turn re-reading its own diff before answering) is
low-value: without external feedback, intrinsic self-correction is evidence-light
and can degrade output
([arXiv:2310.01798](https://arxiv.org/abs/2310.01798)). The valuable version
(run tests/lint/build, feed failures back, retry) is a real verify loop that
depends on the safer-`bash` policy and overlaps the git workflow, so it moves to
Milestone 5. See "Optional verification loop" there.

Acceptance signal: Iris can make a small code/doc change in a real repository,
show the diff, and explain what it changed.

Gate before Milestone 2: tool results must already support structured metadata, so
large outputs can later become handle-backed without changing every caller.

## Milestone 2 — Token-Efficiency Proof

**Goal:** prove the first unique Iris thesis with measurement.

Potential scope:

- Content-addressed store.
- Handle-returning large tool outputs.
- Micro-summary schema for large results.
- Selective handle dereferencing.
- Token accounting per turn.
- Comparison against naive transcript-passing.

Acceptance signal: a benchmark shows that handle-returning tool outputs reduce
prompt tokens without reducing task success on at least one realistic workflow
such as large search results, large test logs, or multi-file inspection.

Gate before Milestone 3: token accounting and the handle/result shape must be
stable enough that the context engine can build on them rather than replace them.

## Milestone 3 — Context Engine MVP

**Goal:** turn token efficiency from a demo into a core runtime behavior.

Potential scope:

- Context budget planner.
- Context ledger.
- Reason-based context inclusion and eviction.
- Diff-aware file context.
- Cache-aware prompt layout for one or two providers.
- Cache hit/miss and cost reporting where provider APIs expose it.

Acceptance signal: Iris can explain why each major prompt item was included and
can reduce context by policy rather than blind truncation.

Gate before Milestone 4: context inclusion, tool permissions, and model/provider
selection must be centralized in Nexus rather than duplicated in Iris CLI.

## Milestone 4 — Modes and Delegation

**Goal:** add the pi-mmr-inspired workflow only after the base agent and context
engine are stable.

Potential scope:

- Simple mode profiles.
- Mode-specific prompt/tool/model settings.
- Subagents as tools.
- Per-worker model routing.
- Per-worker tool allowlists.
- Worker token/turn budgets.
- Handle-returning worker outputs.

Acceptance signal: Iris can delegate search/review/research work without bloating
the main conversation and can report the token/latency cost of that delegation.

Gate before Milestone 5: worker permissions and tool execution must go through the
same Nexus safety policy as the main agent.

## Milestone 5 — Git-Centered Workflow

**Goal:** make the diff the central deliverable.

Potential scope:

- Diff view after every file change.
- Checkpoint/rollback.
- Dirty-tree detection.
- Per-hunk staging.
- Optional auto-commit behind explicit approval.
- Worktree support.
- Optional verification loop (moved from Milestone 1): after a change, run the
  project's test/lint/build command and feed failures back before the final
  answer. External-signal driven, not an LLM self-critique pass; depends on the
  safer-`bash` policy.

Acceptance signal: Iris can safely complete a local coding task, show the diff,
and either roll it back or prepare it for commit without touching unrelated user
changes.

Gate before Git automation: dirty-tree behavior, rollback semantics, and approval
requirements must be specified before auto-commit, worktree, GitHub, or CI features
are implemented.

## Architecture work — Tier-Boundary Enforcement

**Goal:** make the code match the three-tier ownership split in
[`ARCHITECTURE.md`](ARCHITECTURE.md) — Nexus (core) / Wayland (harness) / Iris
(CLI) — by inverting the outward dependencies in `src/nexus.rs` so the core loop
imports nothing from the tiers above it. This is enabling refactor work, not a
feature milestone; it unblocks clean subagent/mode seams in Milestone 4 and a
later crate split.

Scope is the four cuts. Each is a behavior-preserving refactor and must keep the
Milestone 0/1 verification gates green.

1. **Loop emits events, not UI calls.** Generalize the core sink (today
   `TurnSink`) into the full agent-event stream and remove `use crate::ui` from
   `nexus.rs`. The CLI subscribes and renders. Moves: every `ui.emit(UiEvent::..)`
   in the loop and `UiTurnSink` to Tier 3 (Iris).
2. **Approval becomes a hook.** Define a `BeforeToolCall` gate trait in core; the
   CLI supplies the prompting implementation. The loop calls the gate and keeps
   enforcing policy (`session_allowed`, destructive re-prompt, bash-never-always);
   only `ui.request_approval` and `ApprovalDecision` import move to Tier 3.
3. **Tools become injected.** Define a `Tool` trait and a core `ToolRegistry`;
   build the registry at Tier 3 and inject it into the agent instead of calling
   `crate::tools::{dispatch, ToolState}` directly. Tool impls plus
   `diff_preview`/`requires_approval`/`is_destructive` move to Tier 3; tool
   metadata becomes `Tool`-trait methods on the contract. This registry is
   needed for modes, subagents, and provider-specific tool routing regardless;
   a possible plugin system (issue
   [#18](https://github.com/5omeOtherGuy/iris-agent/issues/18)) would be one
   optional consumer of the same seam, not a reason to build it and not a
   dependency of this refactor.
4. **Persistence is harness-tier.** Move `SessionLog`, `attach_session_log`,
   `persist_new_messages`, and the `workspace` execution surface out of the bare
   core loop into Wayland (Tier 2), or behind a Tier 1 event subscriber.

Acceptance signal: `src/nexus.rs` imports nothing from `crate::ui`,
`crate::approval`, `crate::session`, or concrete `crate::tools::*` impls, and the
existing tests plus the MVP smoke test still pass.

Gate: keep these as in-crate module boundaries (per `AGENTS.md`); do not split
into separate crates until a second front-end or published Nexus runtime
justifies it. Sequence cut 1 first (smallest, unblocks 2-3).

## Roadmap principles

- Build the smallest working layer before adding the next one.
- Do not market token-efficiency claims until measured.
- Keep destructive actions behind explicit approval.
- Prefer one excellent local workflow over many partial integrations.
- Keep safety and tool execution centralized in Nexus; do not duplicate policy in
  Iris CLI, subagents, or future integrations.
- Prefer standard Rust/runtime libraries and established crates for paths,
  processes, JSON, HTTP, and CLI parsing instead of hand-rolled equivalents.
- Treat `PITCH.md` as direction, `FEATURES.md` as inventory, and this roadmap as
  execution order.

## Immediate next steps

1. Done (2026-06-15): real-provider MVP smoke test recorded under the MVP
   verification gate above. All Milestone 0 acceptance criteria and verification
   gates are now met.
2. Resolved: EOF/`/exit` plus a graceful two-stage SIGINT handler
   (`src/signals.rs`) satisfy the MVP exit gate.
3. Milestone 1 is complete (2026-06-15): every scope item is shipped or
   explicitly resolved -- terminal UX/streaming, transcript persistence,
   provider/model config file, `edit` semantics at pi parity, the Landlock
   `bash` sandbox, and the git-diff decision (preview is the single surface;
   post-change display moves to Milestone 5). The acceptance signal is met:
   Iris can make a small change in a real repo, show the diff, and explain it.
   Deferred to a future full-TUI milestone (raw mode): interactive block
   expansion, `Alt+Enter` multi-line editing, and box framing.
4. Done: the tier-boundary cuts are shipped (see [`ARCHITECTURE.md`](ARCHITECTURE.md)).
   Protect that split during runtime work: Nexus stays the bare runtime, Wayland
   stays the harness, and Iris CLI stays terminal I/O/adapters.
5. Next: Runtime completion (the section above). Convert the provider/tool loop
   to the Codex-style async stream/cancel/tool runtime while preserving pi-mono's
   clean contract shape. This gates Milestone 2 because context/token work should
   build on the finished loop, not on the current between-step cancellation model.
6. After runtime completion: Milestone 2 (token-efficiency). The metadata gate is already met
   ([#15](https://github.com/5omeOtherGuy/iris-agent/issues/15) MVP), so the
   remaining work is handle-backing large tool outputs and token accounting.
   First implement shared tool infrastructure in dependency order: path identity and
   observation store ([#11](https://github.com/5omeOtherGuy/iris-agent/issues/11)),
   mutation preflight ([#12](https://github.com/5omeOtherGuy/iris-agent/issues/12)),
   atomic queue/refresh completion ([#13](https://github.com/5omeOtherGuy/iris-agent/issues/13)),
   approval/diff UX ([#14](https://github.com/5omeOtherGuy/iris-agent/issues/14)),
   and result metadata ([#15](https://github.com/5omeOtherGuy/iris-agent/issues/15)).

## Implementation notes backlog

These topics are intentionally not specified in this roadmap, but should be
resolved in focused implementation notes before the relevant milestone starts:

- `NEXUS_MVP_DESIGN.md` — Nexus/Iris CLI boundaries, provider-neutral messages,
  provider interface, tool registry, and approval policy. The three-tier
  ownership target and the four dependency-inversion cuts are specified in
  [`ARCHITECTURE.md`](ARCHITECTURE.md).
- `TOOL_CONTRACTS.md` — input schemas, result/error format, and per-tool behavior
  for `read`, `write`, `edit`, and `bash`.
- `SAFETY_MODEL.md` — workspace path safety, shell limits, approval gates,
  destructive-action policy, and secret handling.
- `BENCHMARK_PLAN.md` — token/cost/latency/task-success measurements for the
  handle-based token-efficiency proof.
