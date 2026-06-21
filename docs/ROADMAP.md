# Iris — Roadmap

> Status (2026-06-17): Milestone 1 and the async-hard runtime completion are
> done. Iris has a text-only session loop, selectable Mimir providers
> (`openai-codex`, `anthropic`, and `antigravity`), streamed response parsing,
> workspace-scoped tools, terminal approval gates with diff previews,
> provider/model settings, and a best-effort JSONL read/write session-store
> foundation. Nexus
> now runs a tokio async loop with turn-level cancellation:
> the provider is an async stream raced against cancellation, tools are async
> with child tokens, concurrency-safe tools run in parallel while everything else
> stays exclusive, and the transcript stays valid on abort. The next runtime work
> is Milestone 2 (token/context). This roadmap defines build order and acceptance
> criteria. `FEATURES.md` remains the capability inventory; this document says
> what to build first.

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
- Incremental terminal streaming of assistant text via the async
  `ChatProvider::respond_stream` → `Stream<ProviderEvent>` contract, rendered as
  `UiEvent` deltas.
- Provider-neutral `ChatProvider`, `AssistantTurn`, `ToolCall`, `Message`, and
  `Role` types.
- Async tokio agent loop with a per-turn `CancellationToken`: provider stream /
  tool / approval reads raced against cancellation, async tools with child
  tokens, safe-parallel batching of concurrency-safe tools, and a valid
  transcript on abort.
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
- Mimir provider auth/token loading for OpenAI Codex, Anthropic Claude Code
  subscription OAuth reuse, and Antigravity Google OAuth.
- OpenAI Codex browser and device-code login flows; Antigravity browser PKCE
  login; Anthropic instructions for reusing an existing Claude Code login.
- OpenAI Codex Responses, Anthropic Messages, and Antigravity/Gemini Code Assist
  request/response handling, including tool schemas and streamed-response
  parsing.
- Harness-owned system-prompt / project-instruction assembly
  ([#56](https://github.com/5omeOtherGuy/iris-agent/issues/56)): the Tier-2
  Wayland `system_prompt::assemble` builds base instructions + runtime context +
  the workspace-root `AGENTS.md` (path-safe, missing-file tolerant) in one place;
  fresh and resumed sessions feed the same assembled string through the existing
  provider request path. Nested/ancestor/global `AGENTS.md`, skills, and prompt
  templates are deferred (skills/templates are issue #57).
- Unit tests for the REPL, tool loop, approvals, tool implementations, path
  safety, atomic writes, auth-file handling, URL/request shaping, and response
  parsing.

Not implemented yet:

- Persistent approval policies, session `/resume` and transcript-tree branching,
  modes, subagents, context ledger, content handles, git automation, and GitHub
  integration.

## Runtime completion — finish Nexus before Milestone 2 [SHIPPED 2026-06-17]

**Goal:** finish the agent runtime, not just the feature checklist. Nexus should
keep pi-mono's clean contracts-in/events-out shape while adopting the mature
Rust async mechanics used by Codex CLI.

**Status: shipped.** Nexus runs a tokio current-thread runtime (`run_session`
owns it and `block_on`s each turn). `ChatProvider::respond_stream` yields a
`Stream<Item = Result<ProviderEvent>>`; the live provider backs it with
`spawn_blocking` + a `futures` unbounded channel (the existing blocking
reqwest/SSE code is unchanged, just wrapped), mirroring Codex's `map_response_
events` minus the transport/telemetry machinery. Each turn owns a
`tokio_util::sync::CancellationToken`; `tokio::select!` races every provider
stream read and every tool future against it, tools receive a `child_token()`,
and a Ctrl-C watcher thread bridges the async-signal-safe SIGINT atomic onto the
token (the two-stage force-quit/reap handler is untouched). Tools are async
(`Tool::execute` returns a boxed future) and classify themselves via
`is_concurrency_safe`; the loop runs consecutive concurrency-safe, ungated calls
in parallel with bounded ordered buffering and everything else
exclusively. The concurrency-safe built-ins (`grep`/`find`/`ls`) run their
blocking body on `spawn_blocking` and await the handle, so a parallel batch is
genuinely concurrent and the future yields for the cancellation race; `read`
stays exclusive because it mutates the read-before-write state behind the env's
`!Send` RefCell. On cancellation every model-emitted call still gets a real or
synthetic cancelled tool result, so the next request stays valid. Long blocking
work is cooperatively cancellable: one-shot bash kills its process group, and
persistent-session `run` and background-job `finalize` poll the turn token while
waiting; a cancelled approval is recorded as cancelled (never a silent deny) and
cannot mutate the session allow-policy; the provider's blocking task checks the
token before each attempt, across retry backoff, and between SSE lines. The bare
`Agent` depends on no UI/signals/concrete tools. Verified: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test` (245 tests, 10 new
runtime tests covering streaming order, stream cancel, async tool feed, tool
cancel, sequential default, safe parallel, unsafe exclusivity, approval cancel,
and bash session/job cancellation).

Deferred (bounded, documented in code with `ponytail:` markers):
- **Real-terminal approval cancellation is NOT implemented.** `UiBridge::review`
  does a blocking stdin read; the single-threaded executor cannot preempt it, so
  the first Ctrl-C does not interrupt a *pending terminal* prompt (the loop
  races the approval against the token, and once the read returns a late
  decision is discarded, but the read itself blocks until the second Ctrl-C
  force-quits at the process level). The loop-level cancellation race is proven
  with a cancellable gate (`loop_cancels_a_pending_approval_with_a_cancellable_gate`).
  Upgrade path: a non-blocking / single-owner terminal input event loop.
- A cancelled provider stream's `spawn_blocking` HTTP request is preempted
  promptly while actively streaming (token check + dropped-consumer break), but
  an *idle* socket read is not interrupted until the next byte or the 120s
  reqwest timeout (blocking reqwest cannot be force-aborted mid-read); bounded at
  process exit by `Runtime::shutdown_timeout`. Upgrade path: async reqwest.
- A cancelled `grep`/`find`/`ls` is abandoned, not aborted: dropping the
  `spawn_blocking` handle lets the orphaned walk finish on the pool with its
  result discarded. Upgrade path: thread cancellation into the `ignore` walker.

This work was the next sequencing gate before Milestone 2. The previous loop was
clear and already parsed provider streams, but the provider/tool seams were too
blocking for a terminal agent: cancellation was observed between steps rather
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

### Acceptance criteria [all met]

Runtime completion is done only when tests prove (all shipped in
`src/nexus_tests.rs`):

1. [done: `streamed_events_reach_observer_in_order`] A fake streaming provider
   emits multiple events in order and the observer sees them in order.
2. [done: `cancellation_during_provider_stream_exits_promptly_with_valid_state`]
   Cancelling while a fake provider stream is pending exits promptly and leaves
   valid turn state.
3. [done: `async_tool_result_feeds_follow_up_turn`] Async tools are awaited and
   their results feed a follow-up model turn.
4. [done: `cancellation_during_tool_aborts_and_records_valid_result`] Cancelling
   while a fake tool is pending exits promptly and records a valid
   cancelled/error tool result.
5. [done: `unsafe_tools_run_sequentially`] Two unsafe tools do not overlap.
6. [done: `safe_tools_run_in_parallel_with_ordered_results`] Two explicitly safe
   tools do overlap, while their recorded results remain deterministic.
7. [done: `safe_tools_do_not_cross_an_unsafe_tool`] Safe tools do not cross an
   unsafe tool in a way that changes effects or transcript order.
8. [done at the loop level: `loop_cancels_a_pending_approval_with_a_cancellable_gate`]
   A pending approval unblocks on cancellation and is recorded as cancelled (not
   denied). CAVEAT: this holds for any *cancellable* gate; the real terminal
   gate's stdin read is blocking and is NOT cancellable until a non-blocking
   input layer lands (see Deferred above).
9. [done] Existing observer, approval, provider, tool, path-safety, and
   transcript tests remain green. Plus `cancellation_stops_a_session_run_and_recovers`
   and `cancellation_interrupts_finalize_without_killing_job` cover the bash
   session/job cancellation paths (245 total).

Resolved review findings (2026-06-17, post-ship): real-terminal approval
cancellation honestly de-scoped (above); bash persistent-session/finalize waits
now poll the turn token; `grep`/`find`/`ls` made genuinely parallel via
`spawn_blocking` (and `read` correctly marked exclusive); the provider blocking
task made cancellation-aware; and a cancelled approval is recorded as
`Cancelled` rather than `Deny`.

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
| [#15](https://github.com/5omeOtherGuy/iris-agent/issues/15) | Tool output/result/error contract | Done (MVP): `dispatch` returns a `ToolOutput { content, metadata }`; success results carry a per-tool `metadata` object on the wire (`read` byte/line/`truncated`, `ls` entries, `write` bytes, `edit` occurrences). Handle-backing for large outputs shipped ([#61](https://github.com/5omeOtherGuy/iris-agent/issues/61)): oversized results are offloaded out of context behind a stable handle with a compact preview + `outputHandle` metadata. |

Status: strong-standard on the read/grep/edit/write/ls cluster, with `edit` now
on Claude Code's exact-string contract, a shared read-before-mutate stale-file
guard, and a structured `ToolOutput` result/metadata contract. The honest gaps
are `bash` (large), persistent approval policy/risk
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

The planner seam exists ([#60](https://github.com/5omeOtherGuy/iris-agent/issues/60)):
`ProviderCapabilities` (reported by each provider) drives `Tools::plan_surface`,
which narrows the *model-visible* surface (`Tools::iter`) while leaving execution
lookup (`Tools::by_name`) over the full registry untouched. Default capabilities
advertise the full built-in surface, so every provider is unchanged today; the
native-edit replacement tool itself is still future work.

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
  (bracketed paste + `\` continuation). Full-screen TUI shipped later on top
  of this: raw-mode alternate screen, persistent transcript, textarea editor,
  spinner, slash palette, and Ctrl-C/input edge-case handling, with text UI as
  the non-TTY/piped fallback. Inline-Viewport Native Scrollback
  ([#86](https://github.com/5omeOtherGuy/iris-agent/pull/86), shipped
  2026-06-20) then re-architected the interactive TUI off the alternate screen:
  finalized transcript blocks are committed into the terminal's native
  scrollback (selectable/copyable, scrolled by the real terminal) via ratatui's
  `Viewport::Inline` + `Terminal::insert_before`, above a small fixed live
  viewport (`take_scrollback` keeps the current block live mid-turn and flushes
  everything at idle). The manual scroll offset and its PageUp/PageDown/
  Ctrl+Home/End + mouse-wheel handlers were removed (the terminal owns
  scroll/select/copy). The same slice fixed multi-file diff headers (drop every
  `---`/`+++` pair), removed the false `ctrl + t` transcript hint, made wrapping
  URL/long-token-safe (fits -> own row; over-long -> hard-break, never clipped),
  added flood-safe row-capped tool output, and added markdown rendering for
  assistant text via pulldown-cmark (`src/ui/markdown.rs`, raw/inline HTML text
  preserved). A later shortcut-parity pass aligned the TUI editor/model controls
  with pi defaults: Shift+Enter (plus Ctrl+Enter/Ctrl+J fallbacks) inserts a
  newline, Alt+Enter submits, Ctrl+L opens the model selector, Ctrl+P/
  Shift+Ctrl+P cycles scoped models, Shift+Tab cycles reasoning, and editor
  movement/deletion keys follow pi's defaults. Deferred during this slice and tracked as follow-ups: markdown
  streaming renders raw then snaps to formatted
  ([#87](https://github.com/5omeOtherGuy/iris-agent/issues/87)); markdown
  nested-context gaps -- code in blockquote/list, multi-paragraph list items
  ([#88](https://github.com/5omeOtherGuy/iris-agent/issues/88)); diff colorizer
  false-positive on zero-context (`-U0`) diffs
  ([#89](https://github.com/5omeOtherGuy/iris-agent/issues/89)); larger TUI
  scope ([#90](https://github.com/5omeOtherGuy/iris-agent/issues/90)) -- of which
  sub-item 1, per-command live bash streaming with Running/Ran exit-code+duration
  exec lifecycle cells, shipped in
  [#94](https://github.com/5omeOtherGuy/iris-agent/pull/94) (2026-06-20:
  `ToolStarted`/`ToolOutputDelta` display-only events + `exit_code`/`duration` on
  `ToolResult`, a `ToolOutputSink` seam streaming one-shot bash chunks, and an
  in-place-finalizing exec cell; session-path live deltas, full diff/syntax
  rendering as #90.2, and DIM parity as #90.3 still deferred); and a real-TTY
  smoke verification pass
  ([#91](https://github.com/5omeOtherGuy/iris-agent/issues/91)). Still deferred:
  interactive expand/collapse of folded blocks and full right-bordered box
  framing.]
- Streaming output if not already in MVP. [Shipped: `TurnSink` deltas.]
- Session transcript persistence. [Shipped, now a read/write store foundation:
  `src/session.rs` writes a JSONL transcript -- a `session` header line plus one
  `message` line per entry -- to `<root>/<cwd-slug>/<unix-ms>_<id>.jsonl`, where
  `<root>` is `IRIS_SESSION_DIR` or `~/.iris/sessions`. The harness appends new
  messages after each turn (best-effort: a write failure warns, never crashes
  the session; flushed per line so a crash leaves a valid prefix). Mirrors
  pi-mono's session store at the smallest useful level. Session Store
  Foundation ([#42](https://github.com/5omeOtherGuy/iris-agent/issues/42),
  shipped 2026-06-17) added the tree-ready/read pieces on top of the original
  write-only log: stable session ids (header `id`) and per-entry ids, a
  `parentId` link on every entry (the previous leaf, `null` for the first) so
  future branching can attach to any entry, format version bumped v1 -> v2
  (the reader still accepts v1), and a `SessionStore` read side --
  `list()` returns per-session metadata (id, path, cwd, created/updated ms,
  newest-first) by reading only each header line + mtime, and `open(id)` reads
  a session back with its messages in order (skipping a truncated trailing
  fragment). Tests cover create/open/list/read/append/parent-linkage.
  Deferred (later milestones, intentionally outside this slice): surfacing
  entry ids/`parentId` on read for branching/tree navigation,
  compaction/branch-summary entries, labels, fork, and token accounting. This
  ships the durable, resumable-ready store. Session Resume MVP
  ([#47](https://github.com/5omeOtherGuy/iris-agent/issues/47), shipped
  2026-06-17) builds on it: `iris resume <session-id>` finds the session
  via `SessionStore::find`, reconstructs the prior provider-visible messages
  (`Agent::resumed` seeds the loaded transcript), reopens the same JSONL file
  for append (`SessionLog::resume` restores the leaf link + id counter), and
  the harness continues appending future turns to that same log (a `persisted`
  cursor past the loaded history avoids rewriting it). Errors clearly on an
  unknown id. A focused test
  (`resumed_session_feeds_prior_context_into_next_turn`) proves the loaded fact
  reaches the next model turn and that continuation does not duplicate history.
  Still deferred (outside #47): the in-session `/resume` picker UI, branching,
  rollback, and session search. Context Compaction Foundation
  ([#49](https://github.com/5omeOtherGuy/iris-agent/issues/49), shipped
  2026-06-17) adds the first compaction slice on top of the resume path: a
  durable `compaction` JSONL entry records an inclusive range of covered
  `message` entry ids, the `summary` that replaces them, a `createdAt`
  timestamp, and a `tokenEstimate` placeholder (`null` until a token convention
  exists). A manual/internal append path (`SessionLog::append_compaction`,
  which returns the assigned entry id, as `append` now does) writes one;
  `read_messages` rebuilds context by replacing each covered range with its
  summary in place, so a resumed session sees the summary instead of replaying
  the covered turns, without duplicating them. Coverage is keyed on durable
  entry ids (not array positions); multiple non-overlapping compactions apply
  deterministically, and an overlapping/missing-id range is rejected as invalid
  session data. `resume` treats a compaction entry as the leaf so a continued
  session chains and counts past it. Tests cover the compacted rebuild, the
  unchanged uncompacted resume, multiple compactions, overlap/missing-id
  rejection, and resume-after-compaction. The summary's production is kept
  swappable: storage and rebuild are independent of how the text was made
  (manual now; a provider/local/remote summarizer later), and the role/text of
  the rebuilt summary message lives in one place. Deferred (outside #49,
  intentionally): auto-compaction thresholds, full token-budget policy, branch
  summaries, rollback, and a TUI/CLI compaction command.]
- Focused config file for provider/model/tool policy. [Shipped (provider/model):
  `src/config.rs` loads JSON settings from `~/.iris/settings.json` (global,
  override via `IRIS_CONFIG_PATH`) and `<cwd>/.iris/settings.json` (project).
  Project-local settings may override only `defaultModel`; global/user settings
  own `defaultProvider` (validated; supported ids: `openai-codex`, `anthropic`,
  `antigravity`) and `baseUrl` so a cloned repo cannot redirect bearer tokens.
  OpenAI Codex keeps its existing env override precedence for `IRIS_MODEL` and
  `IRIS_CODEX_BASE_URL`; Anthropic and Antigravity use settings/defaults for
  model/base-url. Unknown keys are ignored, a malformed file errors with its
  path. Tool/approval policy
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

- Content-addressed store. [Foundation shipped as part of
  [#61](https://github.com/5omeOtherGuy/iris-agent/issues/61): oversized tool
  outputs are stored content-addressed (`sha256[..16]`) in a session-sibling
  `<session>.outputs/` directory (`src/handles.rs`).]
- Handle-returning large tool outputs. [Foundation shipped
  ([#61](https://github.com/5omeOtherGuy/iris-agent/issues/61)): a successful
  tool result over a 16 KiB threshold is persisted out of provider context
  behind a stable handle, and the transcript carries a compact head+tail preview
  plus an `outputHandle` metadata pointer (`id`/`bytes`/`lines`) instead of the
  full payload. Nexus owns the threshold/offload policy and a Tier-1
  `ToolOutputStore` contract; the Wayland harness implements it over local
  session storage and injects it via `ToolEnv`. Small outputs keep their exact
  inline encoding; with no durable store (in-memory session) or on a store-write
  failure the full output stays inline rather than being truncated/discarded.
  Because the substitution happens before the message enters context, resume
  rebuilds the compact form for free and never re-inlines the payload. Deferred
  (outside #61): a model-facing dereference tool / TUI attachment browser
  (`HandleStore::get` is the retrieval seam), binary artifacts, and
  search/indexing.]
- Micro-summary schema for large results.
- Selective handle dereferencing.
- Token accounting per turn. [Foundation shipped
  ([#54](https://github.com/5omeOtherGuy/iris-agent/issues/54)): each `message`
  session entry persists a conservative content-derived `tokenEstimate`, the
  read/rebuild path sums them (preferring the persisted value, recomputing from
  content for legacy v1 entries) into `StoredSession.context_tokens`, and a
  compacted range contributes its summary's estimate instead of the covered
  turns -- so a reopened session reports the same context token total it had in
  memory (`session::context_tokens`). A `contextTokenBudget` setting is
  parsed/defaulted. Deferred (outside #54): pricing/cost accounting, and exact
  provider-reported usage (the `tokenEstimate` field is the swap-in point once
  providers surface it).]
  Auto Compaction Foundation
  ([#55](https://github.com/5omeOtherGuy/iris-agent/issues/55), shipped
  2026-06-17) makes `contextTokenBudget` trigger runtime behavior: the Tier-2
  Wayland harness compares the current context token total against the budget at
  each safe turn boundary (before the provider request) and, when it is
  exceeded, compacts via the existing `SessionLog::append_compaction` +
  read-time rebuild. A deterministic internal summarizer (`wayland::summarize`,
  bounded excerpts) stands in for a real summary and is the explicit swap point;
  the harness picks a covered range that keeps the recent tail within budget and
  never splits a tool-call/result pair, then replaces the in-memory context with
  `summary + tail` so the next request uses the summary instead of the covered
  turns. Under-budget sessions never compact; over-budget sessions create a
  compaction entry; resumed sessions rebuild through prior summaries
  (already-loaded history is tracked id-less this slice, so only post-resume
  turns are re-coverable -- a documented ceiling). Tests cover under-budget
  no-op, over-budget compaction at a turn boundary, and resume/rebuild after
  auto-compaction. Deferred (outside #55): provider-generated summaries,
  branch-aware compaction, rollback, a manual TUI/CLI `/compact` command, and
  background/offloaded compaction.]
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
4. **Persistence is harness-tier.** Keep `SessionLog`/`SessionStore`, transcript
   append/read-back, and the `workspace` execution surface out of the bare core
   loop in Wayland (Tier 2), or behind a Tier 1 event subscriber.

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
   Full-screen TUI now covers raw-mode interaction, textarea editing, spinner,
   slash palette, and transcript rendering; interactive block expansion and full
   box framing remain deferred.
4. Done: the tier-boundary cuts are shipped (see [`ARCHITECTURE.md`](ARCHITECTURE.md)).
   Protect that split during runtime work: Nexus stays the bare runtime, Wayland
   stays the harness, and Iris CLI stays terminal I/O/adapters.
5. Done (2026-06-17): Runtime completion (the section above). The provider/tool
   loop is now the Codex-style async stream/cancel/tool runtime, preserving
   pi-mono's clean contract shape, with all nine acceptance tests green. This
   unblocks Milestone 2.
6. Done (2026-06-17): Milestone 2 foundations are in place -- result metadata,
   token estimates and `contextTokenBudget` ([#54](https://github.com/5omeOtherGuy/iris-agent/issues/54)),
   handle-backed large tool outputs ([#61](https://github.com/5omeOtherGuy/iris-agent/issues/61)),
   and turn-boundary auto-compaction ([#55](https://github.com/5omeOtherGuy/iris-agent/issues/55)).
   Next: prove the token-efficiency thesis with benchmark evidence, then add the
   missing consumer slices: selective handle dereferencing, richer micro-summary
   schema, and provider-quality compaction summaries.

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
