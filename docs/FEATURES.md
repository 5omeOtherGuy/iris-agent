# Iris — Feature List

> Status (2026-06-26): Milestone 2 foundations are implemented; the remaining
> Milestone 2 gate is benchmark proof plus consumer slices. Labels:
> **[Implemented]** · **[Partial]** · **[Planned · MVP]** · **[Planned]** ·
> **[Research]**. This file is
> a capability inventory, not a build sequence; use [`ROADMAP.md`](ROADMAP.md) for
> milestone order.

## Core CLI and agent loop

- **CLI entrypoint** — `cargo run` starts Iris. [Implemented]
- **Interactive terminal session** — terminal-surface TUI on real TTYs with an
  Iris-owned transcript replay/diff renderer, textarea editor, spinner, slash
  palette, modal selectors, live bash exec cells, compact tool timers,
  state-specific panel symbols, `ctrl+o` output preview/reveal, word-level diff
  highlights, streamed GFM-style Markdown rendering, collapsed reasoning panels,
  `/exit` and `/quit`; text REPL fallback for pipes/CI or TUI startup failure.
  [Implemented]
- **Conversation state** — in-memory multi-turn user/assistant messages for the
  current process, plus linear session resume from persisted transcripts.
  [Partial]
- **Provider-neutral turn/message shape** — `ChatProvider`, `AssistantTurn`,
  `ToolCall`, `Message`, `Role`, and provider-neutral assistant-reasoning rows
  plus display events in Nexus. [Partial]
- **Provider error reporting** — provider errors print to stderr and the REPL
  continues. [Partial]
- **Streaming responses** — incremental assistant text output via the async
  `ChatProvider::respond_stream` → `Stream<ProviderEvent>` contract, consumed by
  the tokio agent loop and rendered as `UiEvent` deltas. [Implemented]
- **TUI transcript rendering** — assistant text supports Markdown tables,
  task-list checkboxes, strikethrough, themed spans, Unicode-aware
  wrap/truncate behavior, and collapsed `Thinking...` panels for provider
  reasoning when the provider supplies displayable reasoning. Redacted reasoning
  never renders provider-hidden text. [Implemented]
- **Runtime-hard cancellation** — per-turn `CancellationToken`, provider
  stream-vs-cancel race, tool-vs-cancel race, child cancellation per tool, and a
  real-or-synthetic cancelled tool result for every emitted call so the
  transcript stays valid on abort. (Caveat: a blocking terminal approval prompt
  is not preempted until the second Ctrl-C.) [Implemented]
- **Safe parallel tool execution** — sequential by default; consecutive
  concurrency-safe, ungated tools (`grep`/`find`/`ls`) run in parallel with
  bounded ordered buffering while unsafe/mutating tools stay exclusive.
  [Implemented]
- **Session transcript persistence** — best-effort JSONL read/write store:
  `SessionLog` appends v2 transcript entries with stable ids, `parentId`, and
  token estimates, plus compaction and model-selection audit entries;
  `SessionStore` lists/finds/opens sessions, rebuilds context through
  compaction summaries, and `iris resume <id>` continues the same log.
  Branching/rollback and an in-session resume picker are planned later.
  [Partial]

## Providers and auth

- **OpenAI Codex Responses provider** — uses the ChatGPT Codex Responses endpoint
  with tool schemas, retry/backoff, and streamed response parsing, exposed as the
  async streaming `ChatProvider::respond_stream` contract (blocking reqwest/SSE
  code runs on `spawn_blocking` and forwards `ProviderEvent`s over a channel,
  cancellation-aware across attempts/backoff/SSE lines). [Partial]
- **Anthropic Messages provider** — uses the Claude Code subscription OAuth lane
  (bearer token, no `x-api-key`), Anthropic Messages SSE, Claude Code identity
  system block, subscription model matrix, tool schemas, streamed text,
  tool-call assembly, normalized reasoning budgets/effort, redacted diagnostics,
  same-origin reasoning replay, cache-control markers when enabled, and supported
  context-management clear edits. Credentials come from the Iris auth store or an
  existing Claude Code login. [Partial]
- **Antigravity provider** — uses Google OAuth for Gemini Code Assist
  (`v1internal:streamGenerateContent?alt=sse`), project-id discovery/persistence,
  Gemini content/tool mapping, streamed text, tool-call assembly, and normalized
  thinking config. Gemini tool-call `thoughtSignature` values are persisted and
  replayed so follow-up requests after tool use stay valid. The public installed-
  app client ID is decoded at runtime; `ANTIGRAVITY_CLIENT_SECRET` is supplied at
  runtime or injected when building Iris. [Partial]
- **OpenAI Codex OAuth auth-file support** — reads `~/.iris/auth.json` or
  `IRIS_AUTH_PATH`, refreshes expired access tokens, extracts account ID from the
  JWT payload, and rewrites refreshed credentials atomically with restricted Unix
  permissions. [Partial]
- **Anthropic Claude Code credential reuse and login** — runs browser PKCE OAuth
  with manual paste fallback, reads `~/.claude/.credentials.json` (or
  `CLAUDE_CONFIG_DIR/.credentials.json`) when the Iris auth store does not
  already hold Anthropic credentials, and writes rotated tokens back to the same
  source without reshaping or dropping sibling keys. [Partial]
- **Antigravity Google OAuth login** — browser PKCE OAuth callback flow through
  `iris login antigravity`; requires `ANTIGRAVITY_CLIENT_SECRET` unless the
  binary was built with it, and uses the same value for later refreshes. [Partial]
- **OpenAI Codex login** — browser OAuth callback flow and device-code OAuth flow
  through `iris login openai-codex`. [Partial]
- **Provider configuration** — `defaultProvider`, `defaultModel`, and `baseUrl`
  settings; supported provider ids are `openai-codex`, `anthropic`, and
  `antigravity`. Project-local settings may override only `defaultModel`,
  `defaultReasoning`, and `contextTokenBudget`; global settings own provider,
  base-url, and model-cycle scope so a cloned repo cannot redirect bearer tokens
  or silently change which provider a session cycles to.
  OpenAI Codex additionally supports `IRIS_MODEL` and `IRIS_CODEX_BASE_URL` env
  overrides. `contextTokenBudget` configures the auto-compaction threshold,
  `defaultReasoning` sets startup thinking/effort, and `enabledModels` scopes
  Ctrl+P model cycling from global/user config only.
  [Partial]
- **Provider-native prompt cache controls** — global-only `promptCacheRetention`
  supports `none`, `short` (default), and `long`. OpenAI receives
  `prompt_cache_key` and optional 24h retention; Anthropic receives
  `cache_control` markers with optional 1h TTL. Iris records provider usage/cache
  metadata and warns only on proven stable-prefix breaks, not ordinary cold
  caches. [Partial]
- **Anthropic context-management opt-in** — global-only
  `anthropicContextManagement` supports the public clear-tool-use and
  clear-thinking edits; provider-side compact is rejected until Iris can persist
  and replay compaction blocks safely. [Partial]
- **Runtime model and reasoning switching** — `/model`, `/reasoning`, TUI
  provider/model/effort pickers, Ctrl+P/Shift+Ctrl+P model cycling,
  Shift+Tab effort cycling, `/settings`, `/scoped-models`, and session-local or
  persisted defaults at safe turn boundaries. [Implemented]
- **TUI auth selectors** — `/login` and `/logout` modals show no-secret provider
  status and drive existing OAuth/subscription flows where available. [Implemented]
- **Session utility commands** — `/session` (transcript file, id, message counts,
  context-token estimate, active model), `/copy` (last assistant reply to the
  system clipboard via pbcopy/wl-copy/xclip/xsel with an OSC 52 fallback for SSH
  sessions), and `/debug` (pi-mono-style snapshot of the rendered screen and the
  provider-visible context written to `~/.iris/iris-debug.log`; `/dbug` alias).
  `/copy` and `/session` also work in the text fallback. [Implemented]
- **Model catalog** — hand-maintained provider/model list for picker display and
  authenticated-model filtering, including current Codex, Anthropic subscription,
  and Antigravity entries. [Implemented]
- **Additional providers** — OpenAI API, local, or OpenAI-compatible backends.
  [Planned]
- **Provider capability matrix** — per-model context window, cache support,
  tool-call format, reasoning controls, JSON reliability, and image support.
  [Partial]

## Agent Kernel MVP tools

- **Tool-call loop** — send tool schemas, receive tool calls, execute async tools,
  and feed tool results back to the model. There is no fixed default round-trip
  cap; `maxToolRoundtrips` can add a graceful soft cap when configured. Runs on
  the tokio loop with per-turn/per-tool cancellation and safe-parallel batching
  of concurrency-safe calls. [Implemented]
- **`read` tool** — read a workspace text file with offset/limit; rejects
  binary/NUL-containing and invalid UTF-8 files rather than rendering lossy
  text. [Implemented]
- **`read` multimodal inputs** — PDF, notebook, and image reading. Nice-to-have
  explicitly deferred until after the core coding-agent workflow is solid.
  [Planned]
- **`write` tool** — create or overwrite a workspace file with atomic
  same-directory replacement. [Implemented]
- **`edit` tool** — targeted exact-string replacement in an existing file
  (Claude Code-compatible `file_path`/`old_string`/`new_string`/`replace_all`),
  including whitespace-normalized fallback matching and atomic replacement.
  [Implemented]
- **`bash` tool** — run a shell command in the workspace with captured output,
  per-call timeout handling when requested, and nonzero-exit reporting. Iris no
  longer applies a default timeout to every bash call. [Implemented]
- **`grep` tool** — search workspace file contents in-process via the ripgrep
  library crates (no `rg` binary required). [Implemented]
- **`find` tool** — find workspace files in-process via `ignore` + `globset`
  (no `fd` binary required). [Implemented]
- **`ls` tool** — list workspace directory entries, directories first, with an
  optional recursive tree and an optional `long` mode (type marker + human-
  readable size per entry). [Implemented]
- **Tool result/error encoding** — structured success/error responses returned to
  the model, including a per-tool `metadata` object on success (e.g. `read`
  byte/line counts and `truncated`, `ls` entry count). Successful outputs over
  50 KiB are stored out of context behind an `outputHandle` when a session store
  is attached. [Implemented]

## Safety and approvals

- **Workspace path safety** — keep file tools inside the workspace by default,
  including policy for absolute paths, `..`, symlinks, binary files, and large
  files. [Partial]
- **Approval gates** — explicit confirmation for `write`, `edit`, and `bash`
  (every mutating file/shell tool), with denied-call handling. [Implemented]
- **Per-project permission policy** — persistent per-cwd grants (ADR-0027,
  #209): per-tool approval defaults for `write`/`edit` and per-command `bash`
  allows (exact or prefix), stored HOME-owned in `~/.iris/trust.json` keyed by
  canonical directory; `[p]` at the approval prompt persists a grant and
  `/trust` (alias: `/permissions`) lists/toggles/revokes them. Destructive commands always re-prompt
  and are never grantable; a repo-committed file can never grant. Sandbox
  posture is stored per project but not yet enforced. [Implemented]
- **Atomic file replacement** — `write` and `edit` write through a
  same-directory temp file, fsync, rename, cleanup-on-error path, and Unix
  permission preservation on overwrite. [Partial]
- **Bash policy** — cwd, optional per-call timeout, stdout/stderr capture,
  output limits, nonzero-exit handling, process-group cleanup, persistent
  sessions, background jobs, and Linux Landlock confinement where available.
  [Partial]
- **File observation / stale mutation preflight** — session-scoped observation
  store records each file's `{mtime, content_hash}` on read/write/edit; `edit`
  and `write` reject mutating an existing file that was never read or has
  changed since last read, and refresh the observation after each mutation. New
  files may still be created blind. [Implemented]
- **Diff/preview approval UX** — show unified diffs or capped new-file previews
  before mutating file tools. [Implemented]
- **Secret redaction** — redact secrets from stored content and summaries.
  [Planned]
- **Subagent tool permissions** — per-worker tool allowlists. [Planned]

## Token and context engine

These are core to the long-term Iris thesis, but they are not part of the first
Agent Kernel MVP unless a milestone explicitly pulls them forward.

- **Context token estimates and budget trigger** — session entries persist
  conservative token estimates, reopened sessions report rebuilt context tokens,
  and `contextTokenBudget` triggers turn-boundary auto-compaction. [Implemented]
- **Token budget planner** — allocates context across system prompt, tools,
  history, files, summaries, and current task. [Planned]
- **Context ledger** — records why each context item is included and supports
  reason-based eviction. [Planned]
- **Session-scoped content-addressed output store** — oversized tool outputs are
  stored beside the session transcript in `<session>.outputs/` by stable
  truncated SHA-256 handle. Files/web pages/diffs/summaries are not yet covered.
  [Partial]
- **Handle-returning tool outputs** — large successful tool outputs return a
  compact head/tail preview, structured `outputHandle` metadata, and a handle to
  full content. [Implemented]
- **Handle dereferencing** — retrieve stored content by handle on demand.
  [Planned]
- **Micro-summary schema** — deterministic schema for counts, truncation, size,
  and confidence. [Planned]
- **Handle lifecycle** — session-scoped retention with ref-counting or
  pin-on-reference. [Planned]
- **Prompt segment caching** — default-short provider-native cache hints for
  stable prompt segments where providers expose public controls; local KV
  caching and private/provider-specific continuity tricks remain deferred.
  [Partial]
- **Cache-aware prompt layout** — providers receive stable prompt/tool prefixes,
  prompt-cache opt-ins, provider usage/cache metadata, and proven cache-break
  diagnostics. More explicit layout planning remains planned. [Partial]
- **Diff-aware file context** — prioritize git diff, touched files, nearby symbols,
  and recent edits over whole files. [Planned]
- **Provider-specific tool surface planner** — Nexus separates the
  model-visible tool surface from the execution registry; providers can hide a
  built-in from declarations while keeping it runnable for existing transcript
  references. Mode/task-specific planning remains planned. [Partial]
- **Compressed tool schemas** — minimal, strict, provider-compatible tool schemas.
  [Planned]

## Compaction

- **Durable compaction entries** — JSONL `compaction` entries replace covered
  message-id ranges with summary messages during read/resume rebuild. [Implemented]
- **Auto-compaction** — when context estimates exceed `contextTokenBudget`, the
  Wayland harness compacts at safe turn boundaries with a deterministic bounded
  excerpt summary, retaining recent context and preserving tool-call/result
  pairs. Provider-quality summaries, manual `/compact`, and branch-aware
  compaction are planned later. [Implemented]
- **Hierarchical compaction** — layered raw turns, compacted older turns, task
  facts, file-change facts, decisions/blockers, and project memory. [Research]
- **Freshness rules** — mark summaries stale when underlying files change.
  [Research]
- **Verification probes** — measure compaction recall and quality. [Research]

## Prompt assembly

- **Fragment-based system prompt** — Wayland assembles provider-visible
  instructions from in-binary shipped fragments (the single source of truth,
  ADR-0026), project docs (`AGENTS.md`/`CLAUDE.md`), runtime context, and
  generated live-tool blocks. No `.md` fragment files are loaded from disk.
  [Implemented]
- **Fragment ordering** — internal fragments use `name` for XML tags and
  numeric `slot` ordering (`slot: 0` disables). [Implemented]
- **Named slots and selector schema** — replace numeric slots with named slots
  and drive prompt/tool inclusion from resolved provider/model/thinking/mode.
  [Planned]

## Modes

- **Mode switching** — switch named mode profiles mid-session. Runtime
  provider/model/thinking switching exists separately today. [Planned]
- **Switch reuse** — reuse assembled context on switch and change only
  mode-specific prompt/tool segments when possible. [Planned]
- **Mode profiles** — prompt shape, tool set, compaction policy, and model
  preference. [Planned]

## Subagents

- **Subagents as tools** — main agent invokes subagents through Nexus tool
  execution. [Planned]
- **Worker set** — search, advisor/reviewer, repo researcher, task worker, and
  user-defined custom subagents. [Planned]
- **Per-worker model routing** — each worker resolves its own provider/model/
  thinking level without changing the parent. [Planned]
- **Isolated worker context by default** — worker runs a fresh conversation with
  only its task prompt. [Planned]
- **Curated context forwarding** — forward selected context-ledger entries to a
  worker by reference. [Planned]
- **Handle-returning workers** — workers return handle plus micro-summary.
  [Planned]
- **Per-worker budgets** — enforced max turns and token caps. [Planned]
- **Filtered tool access** — worker tool allowlist enforced before inference and
  execution. [Planned]
- **Background fleet** — independent workers run in parallel with live grouped
  status. [Planned]

## Edits

- **Content-hash anchored edits** — model references content-hash anchors instead
  of retyping surrounding lines. Anchor format, duplicate handling, stale-anchor
  detection, fallback, and verification are unspecified. [Research]

## Git

Design for the first slice (dirty-tree safety, checkpoint/rollback, final diff,
verification loop) is accepted in
[ADR-0028](adr/0028-git-workflow-dirty-tree-safety-and-task-checkpointing.md)
and tracked by epic
[#261](https://github.com/5omeOtherGuy/iris-agent/issues/261). Do not re-derive
task boundaries, checkpoint storage, or approval semantics — they are decided.

- **Dirty-state handling** — baseline + attribution ledger; never silently
  overwrite uncommitted work (files or index); per-file per-task approvals.
  Spec: ADR-0028; issue
  [#262](https://github.com/5omeOtherGuy/iris-agent/issues/262). [Planned · MVP]
- **Checkpoint / rollback** — op-log-shaped checkpoint chain under
  `refs/iris/*`; task-scoped rollback restoring only Iris-authored changes.
  Spec: ADR-0028; issue
  [#263](https://github.com/5omeOtherGuy/iris-agent/issues/263). [Planned · MVP]
- **Final diff summary** — net task diff (Iris-authored paths only) as the
  deliverable, TUI + plain-text. Issue
  [#264](https://github.com/5omeOtherGuy/iris-agent/issues/264). [Planned · MVP]
- **Verification loop** — run the project's test/lint/build after changes, feed
  failures back, bounded retries; same shell approval policy. Issue
  [#265](https://github.com/5omeOtherGuy/iris-agent/issues/265). [Planned · MVP]
- **Diff view** — present changes as git diffs. [Planned]
- **Auto-commit** — commit changes with generated messages after explicit
  approval. Gated on ADR-0028's still-binding pre-automation gate. Issue
  [#270](https://github.com/5omeOtherGuy/iris-agent/issues/270). [Planned]
- **Per-hunk staging** — stage and commit logically separate changes separately.
  Issue [#269](https://github.com/5omeOtherGuy/iris-agent/issues/269). [Planned]
- **Pre-commit self-review** — agent reviews its own diff before committing.
  [Planned]
- **Worktree integration** — isolated worktree plus branch per task/run; also a
  subagent isolation primitive. Design ADR tracked in
  [#267](https://github.com/5omeOtherGuy/iris-agent/issues/267) (apply =
  settlement through the dirty-tree choke point; linked worktrees only first;
  registry + lifecycle/GC); implementation tracked in
  [#271](https://github.com/5omeOtherGuy/iris-agent/issues/271), blocked on the
  #267 ADR. Reference:
  [`.iris-reference/grok-worktree-subsystem-spec.md`](../.iris-reference/grok-worktree-subsystem-spec.md).
  [Planned]

## GitHub

- **PR lifecycle** — create, update, and describe pull requests. [Planned]
- **PR review** — read diffs, post inline comments, address feedback, resolve
  threads. [Planned]
- **Issues** — issue → task → linked PR → close on merge. [Planned]
- **CI iteration** — read check status, fetch failing logs, iterate until passing.
  [Planned]
- **Stacked PRs** — dependent PR stacks. [Planned]

## Plugins

> Exploratory, not a committed direction. A plugin system is a possibility we
> are keeping open — for tools, and potentially for other extension points — but
> Iris is **not** being built around it. WASM (Extism on Wasmtime) is one
> candidate backend, tracked in issue #18; a subprocess/stdio plugin protocol is
> another. Nothing here is scheduled, and the core does not depend on it.

If a plugin system is ever added, the likely shape is: built-in tools stay native
and trusted; plugins add new tools (or, with explicit opt-in, shadow a built-in),
run sandboxed with no raw workspace access, and reach the filesystem only through
explicit host capabilities that reuse the existing path-safety checks. It would
plug into the same core `ToolRegistry`/`Tool`-trait seam that modes, subagents,
and provider-specific tools need anyway (see
[`ARCHITECTURE.md`](ARCHITECTURE.md)), so that registry work is justified
independently of whether plugins ever ship.

- **Plugin system (tools and beyond)** — load third-party/custom extensions from
  a manifest. WASM/Extism and a subprocess protocol are both candidate backends;
  the choice is undecided. [Research]
- **Sandboxed plugin capabilities** — if plugins land, they get no raw workspace
  access and reach the filesystem only via explicit host functions that reuse
  Nexus path-safety; mutations would be plan-based and host-applied so plugin
  code never touches the filesystem or renders its own diffs. [Research]
- **Identity-based approval** — if plugins land, approval keys on tool identity
  (`plugin:<id>:<sha256>:<name>`) rather than bare name, so overrides and
  untrusted code cannot inherit a prior trust decision. [Research]

## Repo awareness

- **Current codemap** — source-grounded module map in `docs/CODEMAPS/INDEX.md`.
  [Implemented]
- **Tree-sitter repo map** — ranked-symbol map of the codebase. [Planned]

## Out of scope

- Pi execution modes as product surface: interactive / print-JSON / RPC over JSONL
  / SDK surface.
- SDK / embedding surface for building other agents.
