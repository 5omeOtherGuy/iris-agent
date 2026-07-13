# Iris — Roadmap

> Status (2026-07-12): Milestone 1, the async-hard runtime completion, and the
> Milestone 2 foundations are done. Iris has a terminal-surface TUI with
> Iris-owned transcript replay plus a text fallback, selectable Mimir providers (`openai-codex`,
> `anthropic`, and `antigravity`), runtime model/reasoning switching, streamed
> response parsing, workspace-scoped tools, terminal approval gates with diff
> previews, compact/foldable tool panels, richer assistant Markdown, collapsed
> reasoning panels, fragment-based system-prompt assembly, provider/model/
> reasoning/context/cache settings, linear session resume, JSONL session
> persistence, handle-backed large tool outputs, token estimates, turn-boundary
> auto-compaction, default-short provider-native prompt-cache controls, and
> default-off context-management controls, and Codex-compatible native skills
> with progressive disclosure and explicit/implicit invocation.
> Nexus runs a tokio async loop with turn-level cancellation: the provider is an
> async stream raced against cancellation, tools are async with child tokens,
> concurrency-safe tools run in parallel while everything else stays exclusive,
> and the transcript stays valid on abort. The active gate (2026-07-03) is the
> first Git-Centered Workflow slice (epic
> [#261](https://github.com/5omeOtherGuy/iris-agent/issues/261), ADR-0028) —
> sequenced ahead of the Milestone 2 benchmark proof. The benchmark gate
> (prove the token/handle/compaction path reduces prompt tokens without
> reducing task success) follows it. This roadmap defines build order and acceptance
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
- Terminal-surface TUI with Iris-owned transcript replay/diff rendering,
  textarea editing, slash/modals, streamed Markdown rendering, live bash exec
  cells, compact elapsed timers, state-specific panel symbols, `ctrl+o`
  preview/full-output reveal, word-level diff highlights, collapsed reasoning
  panels, and a text fallback for pipes/CI, driven through a `Ui` front-end seam
  (`src/ui/`, `src/cli.rs`).
- Incremental terminal streaming of assistant text via the async
  `ChatProvider::respond_stream` → `Stream<ProviderEvent>` contract, rendered as
  `UiEvent` deltas.
- Provider-neutral `ChatProvider`, `AssistantTurn`, `ToolCall`, `Message`,
  `Role`, Nexus-owned `provider_turn_id` correlation, and assistant-reasoning
  continuity/display-event types.
- Async tokio agent loop with a per-turn `CancellationToken`: provider stream /
  tool / approval reads raced against cancellation, async tools with child
  tokens, safe-parallel batching of concurrency-safe tools, and a valid
  transcript on abort.
- Provider tool-call loop with optional configured round-trip cap,
  retry/backoff, and structured tool result/error messages.
- Mid-run steering and follow-up messages (pi-mono parity): the composer stays
  live while a turn runs, so the user can queue input without interrupting it.
  Enter queues a steering message (injected before the next provider request,
  after the current round's tool calls), Alt+Enter queues a follow-up (injected
  only when the agent would otherwise stop). Nexus owns the injection points and
  a `SteeringSource` seam; the Tier-3 `SteeringQueue` owns the drain policy; the
  working indicator shows a queued count and Ctrl-C clears the queue. A local
  TUI harness actor keeps slash commands, the settings faceplate, transcript
  inspection, approvals, and cancellation live during provider streaming, tool
  execution, approval review, and compaction; runtime-affecting settings queue
  to the next safe boundary. The text/non-TTY path never steers.
- Typed boundary errors with process exit codes (`src/errors.rs`) and `RUST_LOG`
  tracing to stderr (`src/telemetry.rs`).
- Workspace-scoped built-in tools: `read`, `write`, `edit`, `bash`, `grep`,
  `find`, and `ls`. `edit` follows Claude Code's exact-string contract
  (`file_path`/`old_string`/`new_string`/`replace_all`).
- Harness limits aligned with pi-mono where safe: no default tool-roundtrip cap,
  no default bash timeout, full safe-parallel read-only tool batches, and a
  50 KiB inline display threshold, while memory, capture, approval, and
  workspace-safety rails remain in place.
- Workspace path-safety enforcement for existing and newly written paths.
- Terminal approval prompts with diff previews for file-mutating tools, and
  denied-call handling for `write`, `edit`, and `bash`.
- Atomic same-directory file replacement helper used by `write` and `edit`.
- Text-only `read` rejects binary/NUL-containing and invalid UTF-8 files instead
  of rendering lossy text.
- Runtime `/model` and `/reasoning` switching at safe turn boundaries, with TUI
  provider/model/effort pickers, scoped model cycling, `/settings`, `/login`,
  and `/logout`. The `/settings` faceplate exposes the auto-compaction policy
  (AUTO COMPACT: automatic, warn/start/hard percentage thresholds, retain tail,
  reactive, summarizer, worker input) separately from tool-result compaction,
  with a dim resolved-ladder line, live application at the next boundary, and
  background-job cancellation when automatic compaction is turned off.
- Default-short provider-native prompt-cache settings and diagnostics: OpenAI
  prompt-cache keys/24h retention, Anthropic `cache_control`, provider
  usage/cache metadata, and cache-break warnings only when the stable prefix
  provably changed.
- Anthropic server-side context-management clear edits, a probe-only Anthropic
  compact adapter, and default-off, capability-gated native OpenAI compaction.
  Native entries persist portable summaries beside provider-owned replay blocks;
  unsupported and non-opted-in lanes use the active-provider summarizer.
- Mimir provider auth/token loading for OpenAI Codex, Anthropic Claude Code
  subscription OAuth reuse, Antigravity Google OAuth, Anthropic/OpenAI API keys,
  and dedicated keys for configured OpenAI-compatible endpoints.
- OpenAI Codex browser and device-code login flows; Antigravity browser PKCE
  login; Anthropic browser PKCE login plus Claude Code credential reuse; API-key
  login for Anthropic, OpenAI, and OpenAI-compatible providers; shared
  cancellable loopback OAuth callback plumbing with manual-paste fallback.
- OpenAI Codex Responses, Anthropic Messages, OpenAI Chat Completions,
  OpenAI-compatible Chat Completions, and Antigravity/Gemini Code Assist
  request/response handling, including tool schemas, streamed-response parsing
  where supported, and normalized reasoning/thinking controls where supported;
  Anthropic preserves same-origin reasoning continuity in flattened transcripts,
  and Antigravity round-trips Gemini tool-call `thoughtSignature` continuity.
- Harness-owned fragment/slot system-prompt / project-instruction assembly
  ([#56](https://github.com/5omeOtherGuy/iris-agent/issues/56),
  [#74](https://github.com/5omeOtherGuy/iris-agent/pull/74)): the Tier-2
  Wayland `system_prompt::assemble` builds in-binary shipped fragments +
  generated live-tool blocks + user instructions (`~/.agents/AGENTS.md`, then
  `~/.iris/AGENTS.md`) + dynamic root-to-leaf project docs
  (`AGENTS.md`/`CLAUDE.md`) + runtime context in one place; fresh and resumed
  sessions feed the same assembled string through the provider request path. Native filesystem skills
  are implemented (issue #57); templates remain deferred, and named slots plus
  selector-driven assembly remain open (#76/#73). ADR-0026 made fragments fully
  internal (superseding the #202
  user/repo `.md` loading and its per-project trust gate: no
  `~/.iris/fragments` materialization, no repo `.iris/fragments` loading, no
  fragment-trust prompt); project docs keep loading. ADR-0027 repurposed the
  per-cwd store (`wayland::trust`, `~/.iris/trust.json`, canonical-dir keyed)
  as a persistent project permission policy
  ([#209](https://github.com/5omeOtherGuy/iris-agent/issues/209)): per-tool
  `write`/`edit` grants and per-command `bash` allows (exact/prefix), granted
  via `[p]` at the approval prompt and edited via `/trust` (`/permissions` alias); destructive
  commands always re-prompt and are never grantable; per-project sandbox
  posture is stored but not yet enforced.
- Milestone 2 foundations: structured metadata, typed tool-result contracts,
  token estimates, handle-backed large tool outputs, session-scoped
  content-addressed sidecars, turn-boundary auto-compaction, formal
  correlation-id vocabulary for sessions, message entries, provider turns, tool
  calls, compactions, and output handles, plus typed observability events for
  provider-turn lifecycle, tool lifecycle, compaction metadata, and
  output-handle metadata.
- TUI implementation foundations: reusable `Component`/`Container` composition,
  explicit overlay focus routing, a shared Unicode/ANSI text engine, a built-in
  tool renderer registry, terminal-depth palette degradation, frame-owned pager
  hit targets, grapheme-honest truncation, reactive-only motion with live reduced
  motion, transcript-first focus mode (`/focus`, automatic at the 12-row design
  floor), and an opt-in tmux live-rendering harness for manual visual checks of
  pane rendering. Diff evidence and failed shell output stay open by default;
  settled non-redacted reasoning is collapsible.
- Unit tests for the REPL, tool loop, approvals, tool implementations, path
  safety, atomic writes, auth-file handling, URL/request shaping, and response
  parsing.

Not implemented yet:

- Persistent approval policies, transcript-tree branching/rollback, modes,
  subagents, context ledger/planner, handle dereference UI (browser),
  provider-side compact replay, token-efficiency benchmark proof, git
  automation, and GitHub integration. [The model-facing handle dereference
  tool shipped ([#205](https://github.com/5omeOtherGuy/iris-agent/issues/205)).]

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
  an *idle* socket read is not interrupted until the next byte or a TCP
  keepalive dead-peer reset (blocking reqwest cannot be force-aborted
  mid-read); bounded at process exit by `Runtime::shutdown_timeout`. Upgrade
  path: async reqwest.
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

**Web tools [SHIPPED 2026-07-12].** `web_search` and `read_web_page` ship as an
opt-in, off-by-default egress class (ADR-0058). Per-tool backends
(`webSearchBackend`: native DuckDuckGo / Brave API / Jina Search / trusted
SearXNG; `readWebPageBackend`: native pinned-fetch+extraction / Jina Reader)
resolve once at registry build; a tool is registered only when its backend is not
`off`. Global-only timeout, result, response, and final-read-output limits bound
all paths. Containment: global-only config, per-call approval with allow-always,
one SSRF policy (IANA special-purpose deny tables, ports 80/443, no userinfo)
applied to user and Jina target URLs, a pinned redirect-walking client with a
fail-closed resolver that closes the DNS-rebinding TOCTOU, decompressed-byte
caps, validated search-result URLs, and untrusted-content framing. Keys are
user-configured service credentials (`brave-search`/`jina`) with env fallback.
Output reduction is measured and test-enforced over real captured fixtures
(ADR-0036 rule 5): HTML→Markdown extraction, objective excerpting, and search
render, with the snippet-rich result shape recorded in ADR-0059
([web tools benchmark](benchmarks/web-tools-token-efficiency.md)). Deferred:
concurrency-safe classification + fetch semaphore, short-TTL read cache, local
render engine, and a real-response benchmark for the key-gated Brave/Jina search
backends.

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
        Launch platform matrix: Linux + macOS supported, Windows unsupported
        for now. On non-Linux platforms the shell has no kernel sandbox, so the
        `bash` approval prompt states `unsandboxed` at the decision point
        ([#203](https://github.com/5omeOtherGuy/iris-agent/issues/203)); macOS
        Seatbelt confinement is the follow-up.
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
| [#14](https://github.com/5omeOtherGuy/iris-agent/issues/14) | Diff/preview and approval policy | Done (MVP): Nexus enforces a session allow-policy. Approval offers `[y] once` / `[a] always this session` / `[N] deny`; `always` records the tool name in a Nexus-owned `session_allowed` set so later same-tool calls auto-approve (emitted as `ToolAutoApproved`, never inferred by the UI). Deny stays safe-by-default (empty/invalid/EOF). Diff previews now render colored +/- with relative headers (the `a//abs` double-slash and write-vs-edit path inconsistency are fixed). Cross-session persistence shipped via ADR-0027 ([#259](https://github.com/5omeOtherGuy/iris-agent/pull/259)): per-canonical-cwd project grants in the HOME-owned trust store, fail-closed loads, destructive floors non-persistable, persisted auto-approves emitted as `ToolAutoApproved`; `bash_exact` grants partly cover per-command granularity. Remaining: risk labels. |
| [#15](https://github.com/5omeOtherGuy/iris-agent/issues/15) | Tool output/result/error contract | Done (MVP): tools return `ToolOutput { content, metadata }`; Nexus serializes the provider-visible `ToolResultContract` for success, tool error, denied, and cancelled results; success results carry optional per-tool `metadata` (`read` byte/line/`truncated`, `grep` metrics, `ls` entries, `write` bytes, `edit` occurrences). Handle-backing for large outputs shipped ([#61](https://github.com/5omeOtherGuy/iris-agent/issues/61)): oversized results are offloaded out of context behind a stable handle with a compact preview + typed `outputHandle { id, bytes, lines }` metadata. |

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
Nexus regardless of which variant runs. Design decisions for the edit surfaces
(shared mutation core, per-surface tolerance layers, conditional feedback,
failure-class telemetry) are recorded in ADR-0038.

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
  (bracketed paste + `\` continuation). The TUI shipped later on top of this:
  raw-mode terminal interaction, persistent transcript, textarea editor,
  spinner, slash palette, and Ctrl-C/input edge-case handling, with text UI as
  the non-TTY/piped fallback. Inline-Viewport Native Scrollback
  ([#86](https://github.com/5omeOtherGuy/iris-agent/pull/86), shipped
  2026-06-20) then re-architected the interactive TUI off the alternate screen:
  finalized transcript blocks were committed into the terminal's native
  scrollback (selectable/copyable, scrolled by the real terminal) via ratatui's
  `Viewport::Inline` + `Terminal::insert_before`, above a small fixed live
  viewport (`take_scrollback` kept the current block live mid-turn and flushed
  everything at idle). The manual scroll offset and its PageUp/PageDown/
  Ctrl+Home/End + mouse-wheel handlers were removed (the terminal owned
  scroll/select/copy). The same slice fixed multi-file diff headers (drop every
  `---`/`+++` pair), removed the false `ctrl + t` transcript hint, made wrapping
  URL/long-token-safe (fits -> own row; over-long -> hard-break, never clipped),
  added flood-safe row-capped tool output, and added markdown rendering for
  assistant text via pulldown-cmark (`src/ui/markdown.rs`, raw/inline HTML text
  preserved). A later terminal-surface pass superseded the Ratatui inline
  lifecycle in production: transcript history stays in `Screen` state, Ratatui
  remains a primitives/widget dependency, and resize redraws replay from Iris
  state instead of Ratatui's inline viewport lifecycle. A later shortcut-parity
  pass aligned the TUI editor/model controls
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
  `<root>` is `IRIS_SESSION_DIR` or `~/.iris/sessions`. The harness appends each
  complete provider round trip before the next request and keeps its after-turn
  diff as a final/error backstop (best-effort: a write failure warns, never
  crashes the session; flushed per line so a crash leaves a valid prefix). Mirrors
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
  Session Commands MVP
  ([#201](https://github.com/5omeOtherGuy/iris-agent/issues/201), shipped
  2026-07-02) adds `iris -c` / `iris --continue` for the newest session in the
  current directory, `iris resume` with a TTY picker or plain resumable-session
  list, and in-session `/resume` plus `/new` swaps at a turn boundary. Still
  deferred: branching, rollback, and session search. Context Compaction Foundation
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
  Project-local settings may override only `defaultModel`, `defaultReasoning`,
  and `contextTokenBudget`; global/user settings own `defaultProvider`
  (validated; supported ids: `openai-codex`, `anthropic`, `antigravity`),
  `baseUrl`, and `enabledModels` so a cloned repo cannot redirect bearer tokens
  or silently change the provider/model cycle scope.
  OpenAI Codex keeps its existing env override precedence for `IRIS_MODEL` and
  `IRIS_CODEX_BASE_URL`; Anthropic and Antigravity use settings/defaults for
  model/base-url. Runtime `/model` and `/reasoning` switching can persist new
  global defaults, and `/scoped-models` can persist `enabledModels`. Unknown
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

**Sequencing (2026-07-03): the benchmark proof
([#210](https://github.com/5omeOtherGuy/iris-agent/issues/210)) now follows the
first Git-Centered Workflow slice (epic
[#261](https://github.com/5omeOtherGuy/iris-agent/issues/261), Milestone 5).**
A safe, diff-centric change workflow is a prerequisite for credible benchmark
runs; see [ADR-0028](adr/0028-git-workflow-dirty-tree-safety-and-task-checkpointing.md).

Potential scope:

- Content-addressed store. [Foundation shipped as part of
  [#61](https://github.com/5omeOtherGuy/iris-agent/issues/61): oversized tool
  outputs are stored content-addressed (`sha256[..16]`) in a session-sibling
  `<session>.outputs/` directory (`src/handles.rs`).]
- Handle-returning large tool outputs. [Foundation shipped
  ([#61](https://github.com/5omeOtherGuy/iris-agent/issues/61)): a successful
  tool result over a 50 KiB threshold is persisted out of provider context
  behind a stable handle, and the transcript carries a compact head+tail preview
  plus an `outputHandle` metadata pointer (`id`/`bytes`/`lines`) instead of the
  full payload. Nexus owns the threshold/offload policy and a Tier-1
  `ToolOutputStore` contract; the Wayland harness implements it over local
  session storage and injects it via `ToolEnv`. Small outputs keep their exact
  inline encoding; with no durable store (in-memory session) or on a store-write
  failure the full output stays inline rather than being truncated/discarded.
  Because the substitution happens before the message enters context, resume
  rebuilds the compact form for free and never re-inlines the payload. Deferred
  (outside #61): a TUI attachment browser, binary artifacts, and
  search/indexing; the model-facing dereference tool that consumes the
  `HandleStore::get` retrieval seam shipped separately
  ([#205](https://github.com/5omeOtherGuy/iris-agent/issues/205)).]
- Micro-summary schema for large results.
- Selective handle dereferencing. [Shipped
  ([#205](https://github.com/5omeOtherGuy/iris-agent/issues/205)): a read-only
  model-facing `read_output` tool pages an offloaded output back into context by
  its `outputHandle` id (`handle_id`/`offset`/`limit`), through the same
  line-window + truncation contract as `read` (2000-line / 50 KiB caps, shared
  `render_line_window`). Retrieval is exposed on the Tier-1 `ToolOutputStore`
  contract (`get`), so the tool depends on the contract, not the concrete
  `HandleStore`; a malformed/unknown/expired id returns a tool error (never a
  filesystem escape), and a dereference result over the 50 KiB threshold is
  itself re-offloaded behind a fresh handle (no re-inlining loop). Deferred
  (outside #205): grep-over-handle search within an offloaded output (a
  follow-up), and a TUI handle browser.]
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
  providers surface it). Metrics single-home shipped (2026-07-12): all
  token/usage arithmetic lives in `src/metrics.rs` (`TokenFlows`,
  `TimingStats`, `ratio_percent`, `tokens_per_second`,
  `ResolvedContextBudget`); Nexus measures per-provider-turn timing
  (duration + time-to-first-output) on `ProviderTurnCompleted`; the session
  bar meter, trigger ladder, and `/context` divide by the same resolved
  budget installed via `set_compaction_trigger` and disclosed through
  `ContextDiagnostics::budget_facts`; `/context` gained window-derivation and
  measured session-usage sections; the `/debug` snapshot carries a
  collect-only per-turn usage+timing ledger.]
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
  Recoverable-context stack + compaction benchmark (epic
  [#379](https://github.com/5omeOtherGuy/iris-agent/issues/379), gate issue
  [#372](https://github.com/5omeOtherGuy/iris-agent/issues/372),
  [ADR-0045](adr/0045-benchmark-compaction-on-task-success-and-retention.md),
  accepted). The recoverable-context work landed: coverable resumed prefixes and
  resume ids (#375/#377), the compaction generation ordinal (#374/ADR-0047),
  deterministic structured carry (ADR-0044), the mid-session recall tool
  (#373/ADR-0046), and opt-in microcompaction folds (#378/ADR-0048). The
  compaction benchmark now measures four summarizer arms on the deterministic
  fake-provider lane through the production seam -- `provider`, `excerpts`,
  `provider+carry`, and `provider+carry+microcompaction` -- with token deltas
  reported as ratios (`bench_support::est_tokens`) and minimum-reduction bars
  test-asserted. Each arm clears a 60% covered-range reduction bar on its
  scenario; provider and excerpts converge on a single-message covered range and
  separate as the range grows (excerpts/provider summary tokens climb ~1.0 ->
  ~4.6 from one message to ten turns). Retention is split into retained
  (verbatim in rebuilt context, e.g. carry paths) versus
  recoverable-behind-reference (folded or compacted, reachable via a stub path or
  recall handle), each asserted separately. Reported dimensions: compaction
  generation, covered-range size, and cache economics -- modeled deterministically
  (prefix-divergence, estimated tokens) and anchored by an env-gated live
  Anthropic Claude Code OAuth capture: the post-compaction cache-WRITE side is
  realized (1758/1761 input tokens, 5m tier), the summarization request's
  cache-HIT realized 0 on the short synthetic seed (recorded honestly, not
  fabricated). Report: `docs/benchmarks/issue-372-compaction-retention-slice-b.md`.
  The fold-flush price is measured separately
  ([#400](https://github.com/5omeOtherGuy/iris-agent/issues/400),
  `docs/benchmarks/issue-400-fold-flush-cost.md`): a fold-only flush on a warm
  cache re-bills everything below the fold point (realized 2129
  provider-reported write tokens) against a per-turn saving of the folded body
  -- break-even tens-to-hundreds of turns -- while the same fold at a compaction
  boundary is free (marginal write <= 0, asserted) and a cold cache makes it
  free and immediately profitable. The cache-aware fold scheduler shipped on
  that evidence ([#400](https://github.com/5omeOtherGuy/iris-agent/issues/400),
  [ADR-0051](adr/0051-cache-aware-fold-flush-scheduling.md), PRs #405-#409):
  detection recomputes a pending-fold set every boundary; flushes ride free
  breaks (compaction A1, selection/reasoning switch A2/A3, cold resume A4,
  below-minimum prefix A5, manual `/compact` A6, inferred mid-session cold B
  from a provider-neutral `CacheProfile` seam in mimir) with the watermark as
  the Class C backstop; every flush is trigger-tagged on the fold entry, the
  observer event, and the `/context` breakdown. Per-trigger marginal write <= 0
  is CI-asserted; the Class-B live pair realized a -355-token write delta
  (Anthropic, real 390s idle) and a 317-token input saving (Codex, read-side).
  Configurable tool-result compaction now extends that durable fold path with
  retain-N semantic read dedupe, local age/count clearing, four cache-timing
  modes, and `recall(tool_call_id=...)`. The legacy `microcompaction` alias stays
  conservative/default-off. Optional Anthropic-native clearing maps public
  `exclude_tools`/`clear_tool_inputs`; native and local reducers compose only
  over disjoint tool sets (ADR-0022 addendum, 2026-07-09).
  Phase 3 calibration (persisted per-turn usage, watermark retuning) is
  [#395](https://github.com/5omeOtherGuy/iris-agent/issues/395). Deferred
  (still open, outside #372): the over-budget-no-coverable-range floor and
  estimate-vs-actual token calibration.]
- Comparison against naive transcript-passing.
- Native bash output filtering
  ([#336](https://github.com/5omeOtherGuy/iris-agent/issues/336),
  [ADR-0037](adr/0037-native-output-filtering-for-bash-pass-through.md)). [PR 1
  shipped: a declarative eight-stage filter pipeline in
  `src/tools/bash/filter/` applied after capture and before `truncate_tail`,
  across one-shot runs, persistent sessions, and finalized background jobs.
  Filter definitions are embedded TOML data files (63 vendored from RTK,
  Apache-2.0, with attribution; cargo/npm/git-status filters Iris-authored),
  dispatched on the parsed program+subcommand of the last top-level command
  segment. Quality guards: fail-safe raw fallback on filter error/panic or
  emptied output, short-circuit success messages disabled on non-zero exit and
  gated by `unless` error-guards, error/failure lines exempt from stripping,
  `raw: true` tool-param bypass, exit codes/footers untouched. Benchmarked on a
  committed corpus (`docs/benchmarks/adr-0037-bash-filter-tokens.md`): 68-89%
  token reduction on noisy classes, <10 ms overhead, zero loss of failure
  detail (test-asserted). PR 2 shipped (completes #336): structured Rust
  filters in `src/tools/bash/filter/structured/` for cargo test (per-binary
  pass summaries; failures verbatim), cargo build/check/clippy (clean run ->
  `ok`; diagnostics verbatim), git status (branch + tracking + per-file
  state, hints dropped), git log (compact per-commit lines + `N commits`
  summary), git diff (per-file stats; source hunks verbatim; only lockfile
  churn elided), and npm/pnpm test (jest/vitest summary blocks; failure
  blocks verbatim). Dispatched ahead of the TOML registry at the same seam,
  all guards unchanged; a structured filter that cannot parse its output
  declines to raw. Superseded interim TOML filters retired. Corpus-asserted
  bars: cargo test pass >= 85%, git log >= 60%, git status >= 40%, git diff
  lockfile churn >= 30%, npm/vitest pass >= 60%.]
- Opt-in `read` skim mode
  ([#337](https://github.com/5omeOtherGuy/iris-agent/issues/337)). [Shipped
  (PR [#359](https://github.com/5omeOtherGuy/iris-agent/pull/359)): `skim:
  true` strips comments, docstrings, and blank lines per detected language
  (`src/tools/skim.rs`, extension-keyed rules table) for exploration reads;
  kept lines render with their original line numbers so offsets and follow-up
  full reads stay coherent. Never the default; data formats and unknown
  extensions pass through byte-identical; never-worse and emptied-non-empty
  guards fall back to the full rendering. A skim read records no file
  observation, so `edit`/`write` still require a full read first (ADR-0007).
  Benchmarked on a committed corpus
  (`docs/benchmarks/issue-337-read-skim-tokens.md`): 52-72% token reduction
  on comment-heavy Rust/TypeScript/Python (>= 50% bars test-asserted), <10 ms
  overhead, every kept line and signature verbatim.]
- `grep` per-file output guards
  ([#338](https://github.com/5omeOtherGuy/iris-agent/issues/338)). [Shipped
  (PR [#364](https://github.com/5omeOtherGuy/iris-agent/pull/364)): content
  mode takes an opt-in `maxPerFile` cap that limits matches shown per file and
  summarizes the rest with a `… N more matches in this file` count line
  (`src/tools/grep.rs`). No silent drops: shown matches plus summed omitted
  counts equal the exact total, and the header total plus every matched file
  path always survive (both test-asserted). The cap defaults to unlimited, so
  under-cap results stay byte-identical to prior output. Grouping (path printed
  once per file, `> line│` markers) is asserted parity-or-better than the
  ungrouped `path:line:content` form on every fixture, so the "group only if
  smaller" guard is not shipped -- it would never fire. Benchmarked on a
  committed corpus (`docs/benchmarks/issue-338-grep-output-tokens.md`): the
  per-file cap cuts 88% (cap 5) on a high-match file and 72% (cap 20) on
  long-line matches; grouping alone is a 3-27% reduction; <10 ms overhead.]
- `find` truncation summaries and guarded grouping
  ([#340](https://github.com/5omeOtherGuy/iris-agent/issues/340)). [Shipped
  (PR [#363](https://github.com/5omeOtherGuy/iris-agent/pull/363)): a truncated
  result (caps 2000 lines / 50 KB) now ends with an exact summary carrying the
  total match count and the top directories by omitted-match count, replacing a
  bare `[output truncated]` that forced a blind re-run (`src/tools/find.rs`).
  No matches are dropped without a count. Directory grouping (`dir/ a.rs b.rs`)
  is applied only when it is smaller than the flat listing: the runtime picks
  the smaller of the two forms per result set, so a set that would not shrink
  (one file per directory) passes through byte-identical to the historical flat
  output. Benchmarked on a committed corpus
  (`docs/benchmarks/issue-340-find-compaction.md`): grouping cuts 54% on a
  concentrated `.rs` tree (>= 40% bar test-asserted); singletons stay flat at
  0%.]
- Opt-in `ls` output reduction
  ([#339](https://github.com/5omeOtherGuy/iris-agent/issues/339)). [Open: not
  yet started.]

End-to-end measurement partially landed (issue #210): the plan
(`docs/BENCHMARK_PLAN.md`), the benchmark-only arm switch, three committed
workload fixtures with mechanical success checks, and a re-runnable replay
harness are in tree, reported in
`docs/benchmarks/campaigns/legacy-tokens-per-task/tokens-per-task.md` (the
tool-efficiency suite is migrating into the live-harness T-series; see
`docs/BENCHMARK_PLAN.md`). The
deterministic replay proves the token plumbing on the search/log workflows --
arm A (defaults) spends fewer prompt tokens than arm B (baseline) with 100%
success and zero approval prompts (3.4-9.1% on the current fixtures, a
grep/find-grouping lever). It does NOT yet prove the hard half of the acceptance
signal -- that a real model still completes the task from the reduced context --
because the real-provider headline run (>= 3 real runs per cell) is opt-in and
not yet run (it costs money; the harness and repro command are committed). The
gate stays OPEN until that run lands and shows arm A winning with no success
regression; only then does the README claim ship.

Acceptance signal: a benchmark shows that handle-returning tool outputs reduce
prompt tokens without reducing task success on at least one realistic workflow
such as large search results, large test logs, or multi-file inspection. (Replay
evidence committed; real-provider confirmation pending -- see
`docs/benchmarks/campaigns/legacy-tokens-per-task/tokens-per-task.md`.)

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

**Sequencing (2026-07-03): the first slice of this milestone is pulled ahead of
the Milestone 2 benchmark proof
([#210](https://github.com/5omeOtherGuy/iris-agent/issues/210)).** A safe,
diff-centric change workflow is a prerequisite for credible benchmark runs.
Epic: [#261](https://github.com/5omeOtherGuy/iris-agent/issues/261).

**Design is specified and accepted — do not re-derive it.**
[ADR-0028](adr/0028-git-workflow-dirty-tree-safety-and-task-checkpointing.md)
settles dirty-tree behavior, rollback semantics, approval requirements, task
boundaries (settlement-based), checkpoint storage (op-log-shaped chain under
`refs/iris/*`), index protection, bash detection + snapshot-restore, the
blocking/async performance split, session-end/crash recovery, and the tiered
guarantee language.
[ADR-0052](adr/0052-task-workflow-v2-opt-in-guard-and-integrated-settlement.md)
splits the durable task workflow from the mutation guard; commits and successful
print runs can close tasks; `/checkpoint` becomes a non-settling save point;
accepted tasks destroy their checkpoint refs; task state is carried across
compaction; and the future subagent feature must not be named `task`.
[ADR-0058](adr/0058-configure-mutation-safety-and-require-native-jj-consent.md)
adds the global master switch and requires per-workspace consent before native
jj operation tracking is activated. The pre-automation gate below is
satisfied for the #261 slice; deviations require a superseding ADR, not a fresh
discussion.

Active slice (epic [#261](https://github.com/5omeOtherGuy/iris-agent/issues/261)):

- Dirty-tree detection and unrelated-change safety
  ([#262](https://github.com/5omeOtherGuy/iris-agent/issues/262)) — gates the rest. Done.
  Baseline + attribution ledger + choke-point gate: edit/write route a
  pre-existing dirty target through per-file, per-task approval; bash is
  detect-and-restore around a protected-set snapshot. The native jj backend
  brackets each mutating call with non-snapshotting operation reads: a
  pre-existing external operation refreshes the protected baseline, revokes stale
  dirty-file approvals, and stops before execution; an unreadable operation
  boundary also stops the call. Operations completed inside the call window are
  attributed to that call. Non-repository workspaces degrade with an honest
  notice.
- Task-scoped checkpoint/rollback
  ([#263](https://github.com/5omeOtherGuy/iris-agent/issues/263)) — Done. Op-log-shaped
  git checkpoint chain under `refs/iris/checkpoints/<task-id>/` (plumbing only,
  temporary index), auto-checkpoint over the unsettled diff, rollback of ledger
  paths + user index, durable-workflow settlement ref teardown with orphan-ref
  repair, crash-recovery reconciliation, 30-day expiry, non-git content-snapshot
  fallback, and `/rollback`/`/accept`/`/checkpoint` slash commands.
- Final diff summary as the task deliverable
  ([#264](https://github.com/5omeOtherGuy/iris-agent/issues/264)) — Done.
  Ledger-scoped net diff (one hunk set per file, pre-task baseline → current,
  scoped to Iris-authored paths so user changes never appear), surfaced by the
  `/diff` command and the accept-flow summary; fails closed on an unreadable
  checkpoint rather than showing an empty diff; keeps a source-tree parameter for
  a later worktree-apply review (#267/#271).
- Verification loop (moved from Milestone 1): run the project's test/lint/build
  command, feed failures back, retry bounded; external-signal driven, not an
  LLM self-critique pass
  ([#265](https://github.com/5omeOtherGuy/iris-agent/issues/265)). Done. Explicit
  per-project `verify.command` + `verify.maxAttempts` (no auto-detection); the
  command runs after a turn that changed files as a NORMAL gated shell execution
  (unchanged approval gate, no persistent allow-always per ADR-0010, dirty-tree
  guard protects any build artifacts); failure output is fed back to the model as
  a user message for a bounded retry (each retry only after the model made
  further changes; stop at the cap); honest pass/fail-after-N/skipped events;
  verification never settles the task, so a failed loop stays rollbackable
  (ADR-0028).
- Worktree isolation slice — design ADR accepted
  ([#267](https://github.com/5omeOtherGuy/iris-agent/issues/267), closed;
  [ADR-0035](adr/0035-git-worktree-isolation-and-apply-as-settlement.md)). The
  implementation (#271) is the first mutable-subagent backend slice. Settled
  framing: worktree isolation is Tier 0 of the ADR-0028 guarantee model; apply is
  a guarded parent-workspace mutation through the #262 choke point and is task
  settlement only when the opt-in durable workflow exists; the final diff engine
  (#264) doubles as the apply review artifact. Reference:
  [`.iris-reference/grok-worktree-subsystem-spec.md`](../.iris-reference/grok-worktree-subsystem-spec.md)
  (Grok Build subsystem reference, not an Iris decision). Reserves the future
  subagent `isolation` schema seam; read-write subagents must not ship before
  this isolation/apply backend exists.

Later slices (not in #261):

- **Done (2026-07-04, PR #305):** Task recovery ownership fix
  ([#285](https://github.com/5omeOtherGuy/iris-agent/issues/285),
  [ADR-0030](adr/0030-git-safety-task-ownership-lease-and-mutation-lock.md)) —
  shipped first, alone: multi-process recovery could adopt a live foreign
  task and entangle two agents' chains. Now a per-task flock lease + repo-scoped
  mutation lock; `recover_and_expire()` split into `expire_stale` /
  `recoverable_tasks` / `adopt_task`; recovery claims only lease-free tasks and
  never adopts or lists a live foreign one; explicit selection when more than
  one is recoverable.
- **Done (2026-07-04):** Task identity epic
  ([#286](https://github.com/5omeOtherGuy/iris-agent/issues/286),
  [ADR-0031](adr/0031-task-identity-session-linkage-and-resumable-tasks.md);
  depended on #285) — task records carry an opaque body + session links and the
  session log gains `taskLifecycle` audit entries
  ([#287](https://github.com/5omeOtherGuy/iris-agent/issues/287), PR #306); a
  resume-task picker replaced multi-record auto-adopt
  ([#288](https://github.com/5omeOtherGuy/iris-agent/issues/288), PR #308);
  deterministic cwd-scoped find/read of prior sessions by task
  ([#289](https://github.com/5omeOtherGuy/iris-agent/issues/289), PR #307).
  Enforcement never reads the new metadata; the session log is never a
  recovery input. Deferred to Milestone 4 (#216): subagent-backed session
  summarization and model-generated task titles.
- Task workflow v2 follow-up: adopt-while-active false success
  ([#443](https://github.com/5omeOtherGuy/iris-agent/issues/443)) — adoption
  from `/tasks` must be rejected before claiming an orphan lease or appending a
  recovery checkpoint when this process already owns an active task. Recovery
  rows remain visible but non-adoptable until the current task is accepted or
  rolled back.
- Task workflow v2 spine: guard/workflow split and opt-in config
  ([#444](https://github.com/5omeOtherGuy/iris-agent/issues/444)) — dirty-tree
  protection stays always on, while durable task records, checkpoint refs,
  recovery, badges, lifecycle entries, and task slash surfaces require the
  project `tasks` opt-in.
- Task workflow v2 follow-up: print settlement policy
  ([#442](https://github.com/5omeOtherGuy/iris-agent/issues/442)) — successful
  mutating print runs settle the durable workflow task with disposition `print`;
  provider failure or cancellation leaves the record for recovery.
- Task workflow v2 follow-up: settle on user commit
  ([#445](https://github.com/5omeOtherGuy/iris-agent/issues/445)) — when every
  ledger path is clean, Iris treats the user's commit or full revert as an
  external settlement and removes the durable task state.
- Task workflow v2 follow-up: task-scoped approvals cover bash
  ([#446](https://github.com/5omeOtherGuy/iris-agent/issues/446)) — approved
  dirty paths changed by bash are Iris-attributed and checkpointed; unapproved
  dirty paths still halt and restore.
- Per-hunk staging
  ([#269](https://github.com/5omeOtherGuy/iris-agent/issues/269)).
- Optional auto-commit behind explicit approval
  ([#270](https://github.com/5omeOtherGuy/iris-agent/issues/270); needs its own
  ADR per the still-binding gate).
- Worktree support implementation
  ([#271](https://github.com/5omeOtherGuy/iris-agent/issues/271); first backend
  slice for mutable subagents, based on accepted ADR-0035).
- Advanced subagent worktree backend slices — snapshot fast paths,
  pooling/adoption, and remote restore are desired follow-ups after #271 proves
  linked worktree creation, registry, explicit apply, and guarded removal.

Acceptance signal: Iris can safely complete a local coding task, show the diff,
and either roll it back or prepare it for commit without touching unrelated user
changes. Proven end to end by `epic_261_acceptance_end_to_end` (fake-provider
task over a scratch repo: a clean-file edit + a create leave a dirty tracked file
and an untracked file byte-identical, the net diff is scoped to Iris's paths,
rollback restores Iris's paths byte-identically, and `refs/iris/` is empty after
settlement).

Gate before Git automation: dirty-tree behavior, rollback semantics, and approval
requirements must be specified before auto-commit, worktree, GitHub, or CI features
are implemented. [Satisfied for the #261 slice by ADR-0028 (2026-07-03); still
binding for auto-commit, worktree, GitHub, and CI slices.]

## Milestone 6 — Alt-Screen Pager TUI

**Status: shipped (2026-07-03, PRs [#291]–[#298]).** The rich TUI is a
full-frame alternate-screen pager by default (`tui.altScreen = auto`): a
viewport-pinned session bar, an Iris-owned scrollback pane with follow mode
and in-app scrolling/search, and mouse/clipboard behavior that degrades
honestly — with the inline renderer as the automatic fallback and `--plain`
untouched. Remaining optional affordances (block fullscreen viewer, in-app
text selection) are unscheduled follow-ups.

**Goal:** make the rich TUI a full-frame alternate-screen pager — a
viewport-pinned session bar, an Iris-owned scrollback pane with follow mode
and in-app scrolling, and mouse/clipboard behavior that degrades honestly —
while keeping the inline renderer as an automatic fallback.

**Design is specified and accepted — do not re-derive it.**
[ADR-0029](adr/0029-adopt-alt-screen-pager-tui.md) settles the screen-mode
policy (`alt_screen = auto|always|never` + `--no-alt-screen`, multiplexer
auto-degrade), the render backend (stock ratatui `Terminal` full frames from
the existing `Screen` state), the normal fixed-region layout plus its responsive
focus posture (session bar / scrollback pane / working indicator / composer),
the focus model (Tab toggles panes; typing returns to the prompt; Esc is never
nav), the mouse-capture runtime toggle, and the clipboard ladder (native → OSC
52 → tmux buffer).
Binary-verified reference behavior: `.iris-reference/grok-pager-dossier.md`.

Slices, in order (each landed green through the gate):

- **Backend seam + screen-mode policy** — done ([#291]). Mode seam over
  `TuiUi`/`TerminalSurface`; alt-screen enter/leave with panic-hook + Drop +
  force-quit restore; `tui.altScreen` config + `--no-alt-screen` +
  `IRIS_NO_ALT_SCREEN`; tmux control mode/Zellij/dumb/non-TTY degrade with
  notices. Inline mode bit-for-bit unchanged.
- **Full-frame pager render** — done ([#292]). Full frames through ratatui
  `Terminal` in `?2026` sync blocks; session bar pinned, composer pinned;
  resize = re-render; `TestBackend` golden-frame tests.
- **Scroll state + follow mode** — done ([#293]). Offset-from-top scroll
  state; PageUp/PageDown, Alt+Up/Down line scroll, Home/End;
  follow-by-overscroll; dim `▾ N lines below` indicator; windowed
  O(viewport) render over the wrap cache with a perf gate.
- **Mouse + clipboard** — done ([#294]). SGR mouse capture + wheel scroll
  (`tui.scrollSpeed`); Ctrl+T + `/mouse` toggle restores native select/copy
  (`○ mouse off` statusline hint); clipboard ladder (native → OSC 52)
  unchanged behind `/copy`; `alt_screen` default flipped to `auto`.
- **Responsive focus posture** — done. `/focus` toggles a session-scoped
  transcript-first layout; panes at the 12-row design floor select it
  automatically. An empty composer collapses to one bottom session-readout row,
  typing reveals it with session metadata in the top edge, and submit collapses
  it again. Pager and inline render the same posture; reviews and modals remain
  visible.
- **Capability doctor** — done ([#295]). `/terminal-setup` reports terminal,
  multiplexer, SSH, kitty keyboard protocol, OSC 52/tmux clipboard with exact
  `set -g …` fix lines and the Ctrl+J newline fallback.
- **Pager-only affordances** — scrollback focus (Tab) + entry
  selection/folding done ([#296]); transcript search `/find` with n/N done
  ([#297]); sticky user-prompt headers done ([#298]). Still optional, not
  scheduled: block fullscreen viewer, in-app text selection (the mouse
  toggle covers selection until then).

[#291]: https://github.com/5omeOtherGuy/iris-agent/pull/291
[#292]: https://github.com/5omeOtherGuy/iris-agent/pull/292
[#293]: https://github.com/5omeOtherGuy/iris-agent/pull/293
[#294]: https://github.com/5omeOtherGuy/iris-agent/pull/294
[#295]: https://github.com/5omeOtherGuy/iris-agent/pull/295
[#296]: https://github.com/5omeOtherGuy/iris-agent/pull/296
[#297]: https://github.com/5omeOtherGuy/iris-agent/pull/297
[#298]: https://github.com/5omeOtherGuy/iris-agent/pull/298

Gate: the pager must never lose transcript content that inline mode would
have kept (the retained `Screen` state is the source of truth); every slice
keeps `--plain` and inline fallback working; no pane-rendering change ships
without `TestBackend` frame assertions.

## Milestone 7 — IDE-Grade Transcript

**Status: shipped (2026-07-04, PRs [#334](https://github.com/5omeOtherGuy/iris-agent/pull/334) and [#347](https://github.com/5omeOtherGuy/iris-agent/pull/347)).**
Both slices landed through the gate with pre-merge review: syntect
highlighting via the `HighlightFn` seam (later extended to inferred file and
heredoc languages in tool panels), and spans-first OSC 8 hyperlinks
(inline serialization + pager hit-testing, URI sanitization choke point,
marker-forgery defense). `--plain` byte-identical in both.

**Goal:** code, markdown, and tool output render like an IDE — syntax-highlighted
code blocks and clickable hyperlinks — without breaking the accessible plain
path or the dual-backend render model.

**Design is specified — do not re-derive it.**
[ADR-0033](adr/0033-ratatui-native-adoption-boundary.md) settles the boundary:
highlighting implements the existing `HighlightFn` seam in the markdown
renderer; hyperlinks are spans-first (link targets as span metadata, OSC 8
emitted at serialization — inline surface directly, pager via hit-testing or
cell-splitting, decided in-slice); the plain text UI stays ANSI-free.

Slices, in order:

- **Syntax-highlighted code blocks** ([#324](https://github.com/5omeOtherGuy/iris-agent/issues/324)) —
  `syntect` through `HighlightFn`; design-system palette, lazy-loaded syntax
  sets, unknown languages and `--plain` render exactly as today.
- **OSC 8 hyperlinks** ([#325](https://github.com/5omeOtherGuy/iris-agent/issues/325)) —
  markdown links + workspace `file:line` refs clickable; wrapped links stay
  clickable per physical row; no escape bytes in `Screen` state (unit-tested
  invariant).

Gate: `--plain` output stays byte-identical in both slices; no width or wrap
regressions (wide-glyph tests); every pane-rendering change carries frame
assertions.

## Maintenance — TUI Consolidation Sweep

Findings from the 2026-07-04 full-TUI review, sequenced as independent batches
(each one worktree → gate → PR). Boundary rationale:
[ADR-0033](adr/0033-ratatui-native-adoption-boundary.md).

All six batches shipped 2026-07-04 (PRs
[#327](https://github.com/5omeOtherGuy/iris-agent/pull/327),
[#328](https://github.com/5omeOtherGuy/iris-agent/pull/328),
[#330](https://github.com/5omeOtherGuy/iris-agent/pull/330)–[#333](https://github.com/5omeOtherGuy/iris-agent/pull/333)),
plus post-merge review fixes
([#335](https://github.com/5omeOtherGuy/iris-agent/pull/335): table-cell wrap
contract, tmux probe lifetime deadline).

| Batch | Issue | Scope | Depends on |
|-------|-------|-------|------------|
| 1 | [#318](https://github.com/5omeOtherGuy/iris-agent/issues/318) | Remove dead `ModalKey::Tab` + reserved `ansi_aware` module | — |
| 2 | [#319](https://github.com/5omeOtherGuy/iris-agent/issues/319) | Fold `markdown.rs` width/truncation into `textengine`; fix stale ADR-0006 Cargo.toml comment | #318 |
| 3 | [#320](https://github.com/5omeOtherGuy/iris-agent/issues/320) | Consolidate list selection on `Selector`; one `fuzzy_match`; one right-align helper | #318 |
| 4 | [#321](https://github.com/5omeOtherGuy/iris-agent/issues/321) | Centralize glyph literals in `symbols.rs` | — |
| 5 | [#322](https://github.com/5omeOtherGuy/iris-agent/issues/322) | Shared terminal-capability detector (timeout-guarded tmux probe) | — |
| 6 | [#323](https://github.com/5omeOtherGuy/iris-agent/issues/323) | CLI help/dispatch/palette text polish | — |

Open design question: pager scrollbar vs text indicators
([#326](https://github.com/5omeOtherGuy/iris-agent/issues/326)) — decision
before code.

## Live-streaming — Codex-grade TUI streaming

**Status: shipped (2026-07-05, PRs
[#390](https://github.com/5omeOtherGuy/iris-agent/pull/390),
[#396](https://github.com/5omeOtherGuy/iris-agent/pull/396),
[#398](https://github.com/5omeOtherGuy/iris-agent/pull/398),
[#399](https://github.com/5omeOtherGuy/iris-agent/pull/399),
[#402](https://github.com/5omeOtherGuy/iris-agent/pull/402)).**
Five slices, each worktree → gate → pre-merge review → squash-merge. Closes
[#87](https://github.com/5omeOtherGuy/iris-agent/issues/87) (streamed markdown
snapped to formatted on finalize); the deferred #90 session-path advances by its
tool-input seam only — the live patch-preview UI stays deferred.

**Goal:** the TUI streams assistant text, reasoning, and tool-call construction
smoothly — no raw-then-snap reflow, never a blank status line — while keeping the
single authoritative commit-to-scrollback and the security invariants.

Slices, in order:

- **Assistant-message stream controller**
  ([#87](https://github.com/5omeOtherGuy/iris-agent/issues/87), PR
  [#390](https://github.com/5omeOtherGuy/iris-agent/pull/390)) — newline-gated
  collector + adaptive paced drain + one mutable active tail + table holdback,
  in `src/ui/tui/streaming/`. A streamed markdown table no longer reflows on
  finalize. The chunking policy is lifted from Codex (Apache-2.0, attributed in
  `NOTICE` + per-file SPDX).
- **Provider-neutral live reasoning deltas**
  ([ADR-0050](adr/0050-stream-reasoning-summary-deltas.md), PR
  [#396](https://github.com/5omeOtherGuy/iris-agent/pull/396)) — reasoning
  *summary* deltas stream into a live thinking rail; redacted reasoning text is
  never rendered or reconstructed
  ([ADR-0016](adr/0016-preserve-provider-reasoning-continuity-in-flattened-transcripts.md));
  the final block is persisted exactly once; degrades to the block rail when a
  provider streams no deltas. Nexus + Mimir (OpenAI Codex Responses) + TUI.
- **Freeform tool-input delta seam**
  ([ADR-0039](adr/0039-freeform-tool-input-deltas-are-display-only.md), PR
  [#398](https://github.com/5omeOtherGuy/iris-agent/pull/398)) — display-only
  tool-input deltas that never enter provider context; approval and execution
  use only the completed canonical `ToolCall`. Inert until a freeform tool
  (`apply_patch`/V4A) exists to render, so the live patch-preview UI stays
  deferred ([#90](https://github.com/5omeOtherGuy/iris-agent/issues/90)).
- **Always-visible work-phase state machine** (PR
  [#399](https://github.com/5omeOtherGuy/iris-agent/pull/399)) — a UI-owned
  `WorkPhase` (`src/ui/tui/activity.rs`) drives a provider-neutral status label
  from turn start through thinking / answering / running tool / approval /
  finishing, so the header is never blank or misleading. The approval prompt
  stays the primary surface (no competing working animation).
- **Ordering, cancellation, pager hardening** (PR
  [#402](https://github.com/5omeOtherGuy/iris-agent/pull/402)) — regression tests
  locking the invariants: a tool never renders before the preceding streamed
  lines (FIFO), cancellation commits the partial exactly once and clears the
  tail/queue, the pager visible total counts the active tail, and history trim is
  held while a stream or tool is active. Test-only.

Follow-ups (post-wrap-up, 2026-07-05):

- **Live thinking actually streams on OpenAI** (PR
  [#404](https://github.com/5omeOtherGuy/iris-agent/pull/404)) — the Codex
  Responses request now asks for `reasoning.summary: "auto"`, so the API emits
  the summary deltas the reasoning rail consumes. Slice 3's tests injected SSE
  directly, so the missing request field went unnoticed until end-to-end use.
- **Anthropic reasoning joins the rail** (PR
  [#410](https://github.com/5omeOtherGuy/iris-agent/pull/410)) — Anthropic
  Messages forwards non-redacted `thinking_delta` summaries live through the
  same provider-neutral rail. Redacted thinking is never streamed
  ([ADR-0016](adr/0016-preserve-provider-reasoning-continuity-in-flattened-transcripts.md)),
  the retry gate treats shown reasoning as visible output, and reasoning a
  refusal fallback would discard is withheld until the fallback boundary. Nexus
  dedup is provider-agnostic, so no core/TUI change was needed. Live reasoning
  now covers OpenAI Codex Responses and Anthropic Messages.

Gate: every pane-rendering change carries frame assertions; the `--plain` path is
unaffected; Codex-derived files carry Apache-2.0 SPDX + `NOTICE` while the repo
stays MIT.

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
- Treat `PRODUCT.md` as direction, `FEATURES.md` as inventory, and this roadmap as
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
   missing consumer slices: selective handle dereferencing and a richer
   micro-summary schema. Done (2026-07-02, ADR-0041): provider-quality
   compaction summaries (provider request with subagent and deterministic
   excerpt alternatives), a manual
   `/compact` command in both front-ends, switch-time context-cost advisories on
   `/model`/picker/cycle
   switches, and dropping foreign-origin reasoning from every provider request
   after a model change. Auto-compaction redesign slice 0 (2026-07-10) extracts
   the Tier-2 engine state and worker pipeline, persists summary origin and
   reported worker usage, and adds the double-gated two-lane live-loop baseline
   in `docs/benchmarks/auto-compaction-live-loop.md`. Slice 1 (2026-07-10,
   ADR-0054) replaces the absolute trigger with Mimir-resolved effective windows,
   hybrid provider/local measurement, a warn/start/hard ladder, bounded hard
   wait, deterministic fallback, and a model-backed failure breaker. `/context`
   reports the measurement source, ladder, off state, and worker state. Slice 2
   persists completed provider round trips before the next request and proves a
   crash-mid-turn resume is byte-identical. Slice 3 (ADR-0055) adds the
   `ContextGovernor` seam: ready summaries and hard-tier deterministic relief
   can apply inside long tool loops, steering injects after the swap, governor
   failures never fail the user turn, and active job ranges freeze overlapping
   folds. Slice 4 makes verbatim transcript input the worker default, adds
   bounded overflow shrink-retry and global-only dedicated worker routing,
   threads cancellation through both worker routes, and unifies manual
   `/compact [focus]` with the one-slot pipeline. Slice 5 adds typed
   adapter-level overflow classification, one provider-neutral reactive rewrite
   and resend per round trip, deterministic folds/excerpts/deep-cut recovery,
   an honest second-overflow error, and a two-model-compaction per-turn cap.
   Slice 6 adds the durable `/compaction [generation]` viewer, a foldable TUI
   inspection panel, the muted running chip, `Ready`/`Applied` lifecycle states,
   live job/frozen-fold `/context` detail, and the generation-5 warning. Slice 7
   (ADR-0056) adds the provider-neutral capability seam, additive opaque-block
   persistence, portable cross-provider rebuild, discard-on-selection-change,
   and the Anthropic compact beta adapter. Its Claude Code OAuth probe returns
   `400 invalid_request_error`, so Anthropic does not advertise native support
   and `auto` selects the active-provider summarizer directly. OpenAI native
   compaction pairs the encrypted item with a separate provider-authored
   portable summary. Provider summaries are the default for every model; OpenAI
   native mode is an explicit, warned `compaction.providerNative=auto` opt-in
   whose hard-tier fallback obeys the same switch. The portable worker uses a
   compaction-specific system prompt,
   and unsupported lanes retain the deterministic fallback. Slice 8 adds the default-off,
   project-tunable `request_compaction` model tool. It only schedules the
   existing governor, validates an empty argument object, and consumes one
   request at the next safe continuation boundary even when automatic
   thresholds are disabled. Slice 9 extends ADR-0045 with worker,
   trigger/boundary, focus, recall-loop, cache, and provider-native arms. The
   measured defaults are now 0.60/0.72/0.90 with an 8,000-token retained tail;
   the program closeout gate passed on 2026-07-10 with two consecutive full
   Haiku/Codex protocol runs. All evaluated sessions passed G1–G5 and exact
   resume; run 2 used one permitted, verbatim-recorded Codex G1 timing
   exclusion.
7. Prebuilt-binary distribution ([#199](https://github.com/5omeOtherGuy/iris-agent/issues/199),
   [#233](https://github.com/5omeOtherGuy/iris-agent/issues/233)) is wired and now
   validated locally ([#252](https://github.com/5omeOtherGuy/iris-agent/issues/252)):
   the cargo-dist pipeline builds a real host archive, and `install.sh` plus
   `iris update` self-replace are proven against real archives + checksums and a
   mock release response (`scripts/validate-dist.sh`). Gate: the first public
   release is operator-gated and not yet cut. Next (operator): add
   `CARGO_REGISTRY_TOKEN`, cut `v0.1.0`, and run post-release live acceptance per
   [`RELEASING.md`](RELEASING.md).

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
