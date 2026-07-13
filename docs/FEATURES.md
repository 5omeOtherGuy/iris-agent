# Iris — Feature List

> Status (2026-07-03): Milestone 2 foundations are implemented. The active gate
> is the Git-Centered Workflow slice (epic
> [#261](https://github.com/5omeOtherGuy/iris-agent/issues/261), ADR-0028);
> the Milestone 2 benchmark proof follows it. Labels:
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
- **Alt-screen pager TUI** — full-frame alternate-screen renderer
  ([ADR-0029](adr/0029-adopt-alt-screen-pager-tui.md)): viewport-pinned
  session bar, Iris-owned scrollback with follow mode
  (PageUp/PageDown, Alt+Up/Down line scroll, Home/End, mouse wheel at
  `tui.scrollSpeed` lines/tick, re-engage by overscroll, dim `▾ N lines
  below` indicator), O(viewport) windowed rendering over the wrap cache,
  panic-safe alt-screen restore, mouse capture with a runtime toggle (Ctrl+T
  / `/mouse`; off restores terminal-native select/copy, statusline shows
  `○ mouse off`), clipboard ladder (native tools → OSC 52) behind `/copy`.
  Policy: `tui.altScreen = auto|always|never` (default `auto`),
  `--no-alt-screen`, `IRIS_NO_ALT_SCREEN`; tmux control mode, Zellij, dumb
  terminals, and non-TTY stdio degrade to the inline renderer with a notice.
  The `/terminal-setup` capability doctor is next. [Partial]
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
- **Model-driven user questions** — `AskUserQuestion` pauses for 1–4 structured
  questions independently of approval mode. The docked TUI and text fallback
  support single- and multi-select choices, automatic `Other` input, option
  previews, multi-question review, cancel, and bounded model-visible
  `Chat about this` feedback. [Implemented]
- **Session transcript persistence** — best-effort JSONL read/write store:
  `SessionLog` appends v2 transcript entries with stable ids, `parentId`, and
  token estimates, plus compaction and model-selection audit entries;
  `SessionStore` lists/finds/opens sessions, rebuilds context through
  compaction summaries, and `iris resume <id>` continues the same log. Complete
  provider round trips flush before the next provider request; a final/error
  turn-boundary flush remains the backstop. Branching/rollback and an
  in-session resume picker are planned later. [Partial]

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
  `defaultReasoning`, `contextTokenBudget`, `compactionSummarizer`, and the
  project-safe fields of `compaction`; global settings own provider, base-url,
  model-cycle scope, compaction worker model, and provider-native mode so a
  cloned repo cannot redirect bearer tokens or silently change which provider
  a session cycles to.
  OpenAI Codex additionally supports `IRIS_MODEL` and `IRIS_CODEX_BASE_URL` env
  overrides. `contextTokenBudget` clamps the model-aware compaction window,
  `compactionSummarizer` picks who writes compaction summaries
  (`subagent`/`provider`/`excerpts`), `defaultReasoning` sets startup
  thinking/effort, and
  `enabledModels` scopes Ctrl+P model cycling from global/user config only.
  [Partial]
- **Provider-native prompt cache controls** — global-only `promptCacheRetention`
  supports `none`, `short` (default), and `long`. OpenAI receives
  `prompt_cache_key` and optional 24h retention; Anthropic receives
  `cache_control` markers with optional 1h TTL. Iris records provider usage/cache
  metadata and warns only on proven stable-prefix breaks, not ordinary cold
  caches. [Partial]
- **Provider-native compaction** — global-only
  `compaction.providerNative` accepts `off` (default) or capability-gated `auto`.
  `auto` is an explicit OpenAI opt-in and emits a startup warning: the opaque
  encrypted continuation block is reusable only by the same OpenAI model; after
  a model switch Iris uses the separately generated portable summary, whose
  differences may change subsequent behavior. Entries persist a portable
  summary plus one adapter-owned opaque block; only the same adapter and exact
  model replay the block. Anthropic's
  compact beta adapter remains probe-only after the Claude Code OAuth lane
  returned `400 invalid_request_error`; `auto` selects the portable worker.
  OpenAI's native trigger persists the encrypted compaction item and pairs it
  with a separate OpenAI-authored portable summary. Rejected OpenAI model
  capabilities are cached for the process and fall back to portable provider
  summarization. (ADR-0056)
  [Partial]
- **Anthropic context-management opt-in** — global-only
  `anthropicContextManagement` supports the public clear-tool-use and
  clear-thinking edits. The normalized compact route is
  `compaction.providerNative`; the legacy duplicate setting remains rejected.
  [Partial]
- **Recoverable tool-result compaction** — default-off
  `toolResultCompaction` composes retain-N stale-read dedupe with local
  age/count clearing. Shared count/token guards protect the recent working set;
  durable fold entries preserve provider tool-pair invariants; originals remain
  recoverable from session JSONL with `recall(tool_call_id="...")`. Four cache
  timing policies choose explicit breaks, inferred-cold windows, pressure, or
  immediate safe boundaries. The legacy `microcompaction` setting resolves to
  the ADR-0048 conservative `toolResultCompaction` policy and retains its
  independent 64,000-token watermark. [Implemented]
- **Anthropic-native tool clearing** — explicit `anthropicNative` or `auto`
  backends map the public `clear_tool_uses_20250919` trigger, keep, minimum,
  excluded-tool, and tool-input controls. Provider selection rejects overlapping
  local/native tool sets; `auto` falls back to local when native clearing is
  unsupported or cannot honor the configured safety policy. [Implemented]
- **Runtime model and native-reasoning switching** — one typed per-model capability
  map drives provider request construction, startup validation, `/model` and
  `/reasoning`, the TUI provider/model/effort picker, Ctrl+P/Shift+Ctrl+P model
  cycling, Shift+Tab effort cycling, `/settings`, `/scoped-models`, and
  session-local or persisted defaults at safe turn boundaries. It distinguishes
  OpenAI Responses `reasoning`, Chat Completions `reasoning_effort`, Anthropic
  manual budgets vs adaptive effort, and Gemini `thinkingConfig`. Unsupported
  fields are omitted; switches preserve supported effort or show the clamped
  fallback. `/model` reports supported levels, active level, and wire behavior.
  [Implemented]
- **Token-efficient switching** — switches classify as reasoning-only (prefix
  unchanged, silent), model change, or provider change; a model/provider switch
  carrying a large context (over a quarter of the budget) advises `/compact`
  before the new model re-reads it uncached, and foreign-origin reasoning rows
  are never replayed to any provider after a switch. (ADR-0041) [Implemented]
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
- **Provider capability matrix** — typed per-model native-reasoning controls are
  implemented and shared by validation, UI, transitions, and request adapters.
  Context window and cache metadata are model-aware; tool-call format, JSON
  reliability, and image support are not yet one unified matrix. [Partial]

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
  library crates (no `rg` binary required). Content mode groups matches under
  each file path and takes an opt-in `maxPerFile` cap that summarizes overflow
  matches per file with an exact count line (shown plus omitted equals the
  total; nothing dropped silently). Measured on a committed corpus
  (`docs/benchmarks/issue-338-grep-output-tokens.md`). [Implemented]
- **`find` tool** — find workspace files in-process via `ignore` + `globset`
  (no `fd` binary required). A truncated result ends with an exact total match
  count and the top directories by omitted-match count; results group by
  directory when that is smaller than the flat listing. Measured on a committed
  corpus (`docs/benchmarks/issue-340-find-compaction.md`). [Implemented]
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
  and `contextTokenBudget` clamps auto-compaction at turn edges and continuing
  provider-round-trip boundaries. [Implemented]
- **Token budget planner** — allocates context across system prompt, tools,
  history, files, summaries, and current task. [Planned]
- **Context ledger** — records why each context item is included and supports
  reason-based eviction. [Planned]
- **Session-scoped content-addressed output store** — oversized tool outputs are
  stored beside the session transcript in `<session>.outputs/` by stable
  truncated SHA-256 handle. Files/web pages/diffs/summaries are not yet covered.
  [Partial]
- **Native bash output filtering** — captured command output is reduced at one
  seam before `truncate_tail` and the transcript: structured Rust filters for
  cargo test/build/check/clippy, git status/log/diff, and npm/pnpm test
  (jest/vitest); declarative TOML filters for ~60 more commands. Fail-safe raw
  fallback, failure detail verbatim, `raw: true` bypass; reduction measured on
  a committed corpus (`docs/benchmarks/adr-0037-bash-filter-tokens.md`,
  ADR-0036/0037). [Implemented]
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
- **Auto-compaction** — Mimir resolves the selected model's effective window;
  Wayland combines provider usage with estimates for unseen messages and applies
  a configurable warn/start/hard ladder before, during, and after turns. Between
  provider round trips, Nexus consults a provider-neutral governor only after
  complete tool-call/result groups and before steering injection. Ready workers
  apply without waiting; hard pressure bounds worker wait and falls back to
  deterministic excerpts. An explicit `contextTokenBudget` clamps the resolved
  window. Tuned defaults warn/start/hard at 0.60/0.72/0.90 and retain 8,000
  recent tokens; the committed long-horizon and live evidence separates
  covered-range reduction from total-context reclamation. Active worker ranges
  freeze overlapping folds. Branch-aware compaction remains planned. (ADR-0054,
  ADR-0055) [Partial]
- **Reactive overflow recovery** — Mimir classifies adapter-specific context
  overflow responses into one typed failure. Before visible output, Nexus asks
  the provider-neutral governor for deterministic relief and resends once per
  provider round trip. Wayland applies folds, excerpts, then a 1,000-token deep
  cut while preserving tool pairs and durable recall. A second overflow reports
  the measured context/window and recovery commands. Setting
  `compaction.reactive` to `false` disables the path. (ADR-0055) [Implemented]
- **Background summaries** — `compactionSummarizer` selects the answering
  worker. `compaction.worker.input` defaults to a verbatim transcript request;
  `investigator` enables read-only workspace probes. Transcript overflow
  shrinks the covered slice and retries finitely. An optional global-only
  `worker.model` routes the worker through Mimir. The parent alone validates,
  persists, and applies. Entries record origin, instructions, and reported
  worker token/cache usage. (ADR-0041) [Implemented]
- **Provider-native summary worker** — when explicitly enabled and capability
  is reported, the provider replaces the background summary worker without
  replacing the parent-owned apply path. Portable text, opaque block, origin,
  and usage persist together; selection changes discard native jobs. Anthropic
  is implemented but the required Haiku 4.5 live probe is currently rejected by
  the provider. The OpenAI v2 probe succeeds but returns opaque state only.
  (ADR-0056) [Partial]
- **Model-requested compaction** — project-safe `compaction.modelTool=true`
  advertises `request_compaction`. The tool accepts no arguments, sets only a
  session-local one-shot flag, and reports that work is scheduled. Wayland
  consumes the flag at the next pair-closed governor boundary; the parent-owned
  compaction path remains the only mutation point. Automatic thresholds may be
  disabled independently. [Implemented]
- **Manual `/compact [focus]`** — uses the same one-slot worker pipeline at the
  inter-turn boundary. It attaches to an existing job, supports a bounded focus
  instruction, keeps a small recent tail, and works without a budget.
  [Implemented]
- **Compaction inspection** — `/compaction [generation]` reads durable JSONL
  detail: summary, origin, coverage and token mass, carry, instructions, recall
  handle, and worker/cache usage. The TUI renders one foldable panel. A muted
  `compacting…` status chip follows `Running -> Ready/Applied`; `/context`
  reports live job and frozen-fold detail. Generation 5 emits one degradation
  notice with `/new`, `/compact`, and recall guidance. [Implemented]
- **Hierarchical compaction** — layered raw turns, compacted older turns, task
  facts, file-change facts, decisions/blockers, and project memory. [Research]
- **Freshness rules** — mark summaries stale when underlying files change.
  [Research]
- **Verification probes** — deterministic worker/trigger/boundary/focus and
  recall-loop arms plus a double-gated two-lane live protocol measure blocking,
  reclamation, retention, resume equivalence, usage, and cache economics.
  Provider-native capability failures remain explicit. [Implemented]

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

## Skills

- **Codex-compatible filesystem format** — recursively load `SKILL.md` files
  with validated YAML `name`/`description` metadata, frontmatter short
  descriptions, and optional `agents/openai.yaml` interface, dependency, and
  policy metadata. [Implemented]
- **Repo, user, system, and admin discovery** — scan `.agents/skills` from
  repository root to cwd, legacy `<repo>/.codex/skills`, `~/.agents/skills`,
  existing `$CODEX_HOME/skills` plus its bundled `.system` root,
  `~/.iris/skills`, and administrator roots. Canonical-path dedupe, bounded
  depth/count, symlinked directories, and non-fatal load errors match Codex's
  local loader behavior. [Implemented]
- **Codex config compatibility** — honor `skills.include_instructions` and
  ordered name/path `skills.config` enable rules from Codex's config. Optional
  malformed metadata fails open. [Implemented]
- **Progressive disclosure** — expose name, description, and source path under
  a 2% context budget; load the full `SKILL.md` only after selection. Catalog
  changes inject at turn boundaries without rewriting the system prompt.
  [Implemented]
- **Explicit and implicit invocation** — unique `$skill-name` and
  path-qualified picker mentions inject the selected body; the model can select
  implicitly from descriptions unless `allow_implicit_invocation` is false.
  [Implemented]
- **TUI discovery** — `$` and `/skills` open a searchable selector; selecting a
  duplicate name inserts its exact `skill://` path. Interface display names and
  short descriptions appear when present. [Implemented]
- **Confined resource reads** — a loaded skill extends `read` with its own
  canonical directory only. References work under workspace confinement;
  sibling paths and out-of-workspace mutation remain denied. [Implemented]

## Modes

- **Mode switching** — switch named mode profiles mid-session. Runtime
  provider/model/thinking switching exists separately today. [Planned]
- **Switch reuse** — reuse assembled context on switch and change only
  mode-specific prompt/tool segments when possible. [Planned]
- **Mode profiles** — prompt shape, tool set, compaction policy, and model
  preference. [Planned]

## Subagents

- **Subagent backend contract** — Wayland owns spawn/poll/wait/cancel handles,
  request validation, lifecycle status, budgets, allowlists, output-handle
  fields, and read-only child execution. Issue
  [#460](https://github.com/5omeOtherGuy/iris-agent/issues/460). [Implemented]
- **Mutable subagent backend: worktree isolation** — read-write workers are gated
  on the worktree service from the Git section (#271): linked worktree creation,
  durable registry, progress/lifecycle state, explicit apply, and
  `list/show/rm/gc`. The read-only backend contract is already shipped; mutable
  subagents must not fall back to in-place parent-workspace mutation. [Planned]
- **Advanced worktree backend slices** — snapshot fast paths, worktree
  pooling/adoption, and remote session/codebase restore are desired follow-ups
  after the linked-worktree apply boundary is correct. [Planned]
- **Subagents as tools** — main agent invokes subagents through Nexus tool
  execution under a final model-facing name that is not `task`. [Planned]
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
- **Filtered tool access** — read-only workers enforce capability filters before
  inference and execution; mutable/execute worker filters are planned. [Partial]
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
  [#262](https://github.com/5omeOtherGuy/iris-agent/issues/262). [Implemented]
- **Checkpoint / rollback** — op-log-shaped checkpoint chain under
  `refs/iris/*`; task-scoped rollback restoring only Iris-authored changes, plus
  the user index; settlement ref teardown, crash-recovery reconciliation, 30-day
  expiry, non-git content-snapshot fallback, and
  `/rollback`/`/accept`/`/checkpoint`. `/checkpoint` is a non-settling save
  point; `/accept` accepts the current Iris changes and `/rollback` restores a
  rollback point.
  Spec: ADR-0028 + ADR-0052; issues
  [#263](https://github.com/5omeOtherGuy/iris-agent/issues/263) and
  [#448](https://github.com/5omeOtherGuy/iris-agent/issues/448). [Implemented]
- **Final diff summary** — net task diff (Iris-authored paths only, one hunk set
  per file) as the deliverable, TUI + plain-text, via `/diff` and the accept-flow
  summary; fails closed on an unreadable checkpoint. Issue
  [#264](https://github.com/5omeOtherGuy/iris-agent/issues/264). [Implemented]
- **Verification loop** — explicit per-project `verify.command` (+
  `verify.maxAttempts`, default 3, capped 10; no auto-detection) run after a turn
  that changed files, as a normal gated shell execution under the unchanged
  approval policy (no persistent allow-always per ADR-0010; any build artifacts
  go through the #262 dirty-tree guard). Failure output is fed back to the model
  for a bounded retry — each retry only after the model makes further changes,
  stopping at the cap. Honest pass / fail-after-N / skipped events; a failed loop
  never accepts the task, so it stays rollbackable. Issue
  [#265](https://github.com/5omeOtherGuy/iris-agent/issues/265). [Implemented]
- **Diff view** — present changes as git diffs. [Planned]
- **Auto-commit** — commit changes with generated messages after explicit
  approval. Gated on ADR-0028's still-binding pre-automation gate. Issue
  [#270](https://github.com/5omeOtherGuy/iris-agent/issues/270). [Planned]
- **Per-hunk staging** — stage and commit logically separate changes separately.
  Issue [#269](https://github.com/5omeOtherGuy/iris-agent/issues/269). [Planned]
- **Pre-commit self-review** — agent reviews its own diff before committing.
  [Planned]
- **Worktree integration** — isolated worktree plus branch per task/run; also the
  required mutable-subagent isolation primitive. Design ADR
  [ADR-0035](adr/0035-git-worktree-isolation-and-apply-as-settlement.md) is
  accepted and [#267](https://github.com/5omeOtherGuy/iris-agent/issues/267) is
  closed; implementation is tracked in
  [#271](https://github.com/5omeOtherGuy/iris-agent/issues/271). Read-write
  subagents must not ship without this isolation/apply boundary. Reference:
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
