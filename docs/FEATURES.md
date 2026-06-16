# Iris — Feature List

> Status (2026-06-15): early implementation. Labels: **[Implemented]** ·
> **[Partial]** · **[Planned · MVP]** · **[Planned]** · **[Research]**. This file is
> a capability inventory, not a build sequence; use [`ROADMAP.md`](ROADMAP.md) for
> milestone order.

## Core CLI and agent loop

- **CLI entrypoint** — `cargo run` starts Iris. [Implemented]
- **Interactive terminal session** — REPL-style loop with `/exit` and `/quit`.
  [Partial]
- **Conversation state** — in-memory multi-turn user/assistant messages for the
  current process. [Partial]
- **Provider-neutral turn/message shape** — `ChatProvider`, `AssistantTurn`,
  `ToolCall`, `Message`, and `Role` in Nexus. [Partial]
- **Provider error reporting** — provider errors print to stderr and the REPL
  continues. [Partial]
- **Streaming responses** — incremental provider output. [Planned]
- **Session transcript persistence** — save/reload conversations. [Planned]

## Providers and auth

- **OpenAI Codex Responses provider** — blocking non-streaming request/response
  path using the ChatGPT Codex Responses endpoint, with tool schemas and streamed
  response parsing. [Partial]
- **OpenAI Codex OAuth auth-file support** — reads `~/.iris/auth.json` or
  `IRIS_AUTH_PATH`, refreshes expired access tokens, extracts account ID from the
  JWT payload, and rewrites refreshed credentials atomically with restricted Unix
  permissions. [Partial]
- **OpenAI Codex login** — browser OAuth callback flow and device-code OAuth flow
  through `iris-agent login openai-codex`. [Partial]
- **Provider configuration** — `IRIS_MODEL` and `IRIS_CODEX_BASE_URL`. [Partial]
- **Additional providers** — Anthropic, OpenAI API, Gemini-compatible, local, or
  OpenAI-compatible backends. [Planned]
- **Provider capability matrix** — per-model context window, cache support,
  tool-call format, reasoning controls, JSON reliability, and image support.
  [Planned]

## Agent Kernel MVP tools

- **Tool-call loop** — send tool schemas, receive tool calls, execute tools, feed
  tool results back to the model, and stop after a bounded number of tool
  iterations. [Partial]
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
- **`bash` tool** — run a bounded shell command in the workspace with captured
  output, timeout handling, and nonzero-exit reporting. [Implemented]
- **`grep` tool** — search workspace files through `rg` when available.
  [Implemented]
- **`find` tool** — find workspace files through `fd`/`fdfind` when available.
  [Implemented]
- **`ls` tool** — list workspace directory entries, directories first, with an
  optional recursive tree and an optional `long` mode (type marker + human-
  readable size per entry). [Implemented]
- **Tool result/error encoding** — structured success/error responses returned to
  the model, including a per-tool `metadata` object on success (e.g. `read`
  byte/line counts and `truncated`, `ls` entry count) carried in the
  `ToolOutput` contract. [Implemented]

## Safety and approvals

- **Workspace path safety** — keep file tools inside the workspace by default,
  including policy for absolute paths, `..`, symlinks, binary files, and large
  files. [Partial]
- **Approval gates** — explicit confirmation for `write`, `edit`, and `bash`
  (every mutating file/shell tool), with denied-call handling. [Implemented]
- **Atomic file replacement** — `write` and `edit` write through a
  same-directory temp file, fsync, rename, cleanup-on-error path, and Unix
  permission preservation on overwrite. [Partial]
- **Bash policy** — cwd, timeout, stdout/stderr capture, output limits, exit-code
  handling, and process-group cleanup. [Partial]
- **File observation / stale mutation preflight** — session-scoped observation
  store records each file's `{mtime, content_hash}` on read/write/edit; `edit`
  and `write` reject mutating an existing file that was never read or has
  changed since last read, and refresh the observation after each mutation. New
  files may still be created blind. [Implemented]
- **Diff/preview approval UX** — show unified diffs or capped new-file previews
  before mutating file tools. [Planned]
- **Secret redaction** — redact secrets from stored content and summaries.
  [Planned]
- **Subagent tool permissions** — per-worker tool allowlists. [Planned]

## Token and context engine

These are core to the long-term Iris thesis, but they are not part of the first
Agent Kernel MVP unless a milestone explicitly pulls them forward.

- **Token budget planner** — allocates context across system prompt, tools,
  history, files, summaries, and current task. [Planned]
- **Context ledger** — records why each context item is included and supports
  reason-based eviction. [Planned]
- **Content-addressed store** — stores files, command outputs, web pages, diffs,
  and summaries by hash. [Planned]
- **Handle-returning tool outputs** — large outputs return summary, structured
  metadata, and a handle to full content. [Planned] The `ToolOutput`
  result/metadata contract is the seam this builds on; handles are not yet
  implemented.
- **Handle dereferencing** — retrieve stored content by handle on demand.
  [Planned]
- **Micro-summary schema** — deterministic schema for counts, truncation, size,
  and confidence. [Planned]
- **Handle lifecycle** — session-scoped retention with ref-counting or
  pin-on-reference. [Planned]
- **Prompt segment caching** — reuse stable prompt segments where providers expose
  cache behavior. [Planned]
- **Cache-aware prompt layout** — order stable vs. changing prompt parts per
  provider and report cache hit/miss where APIs expose it. [Planned]
- **Diff-aware file context** — prioritize git diff, touched files, nearby symbols,
  and recent edits over whole files. [Planned]
- **Dynamic tool surface** — expose only tools relevant to the current mode/task.
  [Planned]
- **Compressed tool schemas** — minimal, strict, provider-compatible tool schemas.
  [Planned]

## Compaction

- **Hierarchical compaction** — layered raw turns, compacted older turns, task
  facts, file-change facts, decisions/blockers, and project memory. [Research]
- **Freshness rules** — mark summaries stale when underlying files change.
  [Research]
- **Verification probes** — measure compaction recall and quality. [Research]

## Modes

- **Mode switching** — switch model and thinking/effort level mid-session.
  [Planned]
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

- **Diff view** — present changes as git diffs. [Planned]
- **Checkpoint / rollback** — snapshot before multi-step edits and roll back a
  whole task. [Planned]
- **Auto-commit** — commit changes with generated messages after explicit
  approval. [Planned]
- **Per-hunk staging** — stage and commit logically separate changes separately.
  [Planned]
- **Dirty-state handling** — never overwrite uncommitted work; surface conflicts.
  [Planned]
- **Pre-commit self-review** — agent reviews its own diff before committing.
  [Planned]
- **Worktree integration** — isolated worktree plus branch per task/run. [Planned]

## GitHub

- **PR lifecycle** — create, update, and describe pull requests. [Planned]
- **PR review** — read diffs, post inline comments, address feedback, resolve
  threads. [Planned]
- **Issues** — issue → task → linked PR → close on merge. [Planned]
- **CI iteration** — read check status, fetch failing logs, iterate until passing.
  [Planned]
- **Stacked PRs** — dependent PR stacks. [Planned]

## Plugins

Third-party/custom tools via WASM, tracked in issue #18 (Extism on Wasmtime).
Built-in tools stay native and trusted; plugins add or, with explicit opt-in,
shadow them. First pass excludes raw Wasmtime Component Model/WIT, plugin
signing, and network/shell capabilities.

- **WASM plugin tools** — load third-party tools from a plugin manifest
  (`id` + `.wasm` + tool defs) executed through Extism on Wasmtime. [Planned]
- **Nexus `ToolRegistry`** — Nexus-owned registry owning tool definitions,
  identity, dispatch order (approved override > built-in > plugin tool >
  unknown-tool error), and policy; registers built-ins first, then plugin
  manifests, and rejects duplicate names unless an override is approved.
  [Planned]
- **Identity-based approval** — approval keyed on tool identity
  (`builtin:write`, `plugin:<id>:<sha256>:<name>`) instead of bare name;
  plugin tools and built-in overrides require approval, and mutating plugin
  tools require approval every call. [Planned]
- **Sandboxed capabilities** — plugins get no raw workspace WASI access;
  they call explicit host functions (`host_read`, `host_ls`) that reuse Nexus
  path-safety checks. [Planned]
- **Plan-based plugin mutations** — mutating overrides return a proposed
  mutation (`host_write_plan`, `host_edit_plan`); Nexus renders/applies diffs
  with existing trusted logic rather than plugin-provided previews. [Planned]

## Repo awareness

- **Current codemap** — source-grounded module map in `docs/CODEMAPS/INDEX.md`.
  [Implemented]
- **Tree-sitter repo map** — ranked-symbol map of the codebase. [Planned]

## Out of scope

- Pi execution modes as product surface: interactive / print-JSON / RPC over JSONL
  / SDK surface.
- SDK / embedding surface for building other agents.
