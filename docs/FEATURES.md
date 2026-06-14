# Iris — Feature List

> Status (2026-06): pre-implementation. The repository is a skeleton
> (`src/main.rs`, no dependencies); no feature below is implemented. Status
> labels: **[Implemented]** · **[Prototype]** · **[Planned · MVP]** · **[Planned]**
> · **[Research]**. No item is currently Implemented or Prototype.

## Core CLI
- **Interactive terminal session** — REPL-style agent loop. [Planned · MVP]
- **Core tools** — `read`, `write`, `edit`, `bash`. [Planned · MVP]
- **Streaming responses** — incremental output via provider SSE. [Planned · MVP]
- **Providers** — Anthropic, OpenAI, and Gemini-compatible backends; Anthropic and
  OpenAI first. [Planned · MVP]

## Token & context engine
- **Token budget planner** — allocates the context window across system prompt,
  tools, history, files, summaries, and current task before each model call.
  [Planned · MVP]
- **Context ledger** — records the reason each context item is included; supports
  eviction by reason. [Planned · MVP]
- **Content-addressed store** — stores files, command outputs, web pages, diffs,
  and summaries by hash; deduplicates identical content. Guarantees byte identity,
  not semantic freshness. [Planned · MVP]
- **Handle-returning tool outputs** — large outputs return a summary, structured
  metadata, and a handle to the full content. [Planned · MVP]
- **Handle dereferencing** — caller retrieves full stored content by handle on
  demand. [Planned · MVP]
- **Micro-summary schema** — fixed schema (counts, truncation flag, size,
  confidence); generated deterministically rather than free-written.
  [Planned · MVP]
- **Handle lifecycle** — session-scoped retention with ref-counting /
  pin-on-reference. [Planned]
- **Prompt segment caching** — reuses stable prompt segments (base instructions,
  tool descriptions, repo context, mode rules, provider hints). [Planned]
- **Cache-aware prompt layout** — orders stable vs. changing prompt parts per
  provider for cache reuse; reports cache hit/miss. [Planned · MVP for 1–2
  providers]
- **Diff-aware file context** — prioritizes git diff, touched files, nearby
  symbols, and recent edits over whole files. [Planned]
- **Dynamic tool surface** — exposes only tools relevant to the current mode/task.
  [Planned]
- **Compressed tool schemas** — minimal, strict, provider-compatible tool schemas.
  [Planned]

## Compaction
- **Hierarchical compaction** — layered: recent raw turns, compacted older turns,
  task facts, file-change facts, decisions/blockers, project memory. [Research]
- **Freshness rules** — marks a summary stale when underlying files change.
  [Research]
- **Verification probes** — measure compaction recall/quality. [Research]

## Modes
- **Mode switching** — switch model and thinking/effort level mid-session.
  [Planned · MVP for simple profiles]
- **Switch reuse** — reuses assembled context on switch; changes only
  mode-specific prompt/tool segments; reuses the provider cache prefix when model
  and provider are unchanged. [Planned]
- **Mode profiles** — a mode defines prompt shape, tool set, compaction policy,
  and model preference. [Planned]

## Subagents
- **Subagents as tools** — main agent invokes subagents as native tools. [Planned]
- **Worker set** — search, advisor/reviewer, repo researcher, task worker, and
  user-defined custom subagents. [Planned]
- **Per-worker model routing** — each worker resolves its own provider / model /
  thinking level without changing the parent's. [Planned]
- **Isolated worker context (default)** — worker runs a fresh conversation with
  only its task prompt. [Planned]
- **Curated context forwarding (opt-in)** — forward selected context-ledger
  entries to a worker by reference. [Planned]
- **Handle-returning workers** — workers return a handle + micro-summary; full
  output stays in the store. [Planned]
- **Per-worker budgets** — enforced max turns and token caps per worker. [Planned]
- **Filtered tool access** — worker tool allowlist enforced before inference and
  before execution. [Planned]
- **Background fleet** — independent workers run in parallel with live status and
  a grouped task board. [Planned]

## Edits
- **Content-hash anchored edits** — model references content-hash anchors instead
  of retyping surrounding lines. Anchor format, collision/duplicate-line handling,
  stale-anchor detection, line-based fallback, and verification are unspecified.
  [Research]

## Git
- **Diff view** — present each change as a git diff. [Planned · MVP]
- **Checkpoint / rollback** — snapshot before a multi-step edit; roll back a whole
  task. [Planned · MVP]
- **Auto-commit** — commit each change with a generated message. [Planned]
- **Per-hunk staging** — stage and commit logically separate changes separately.
  [Planned]
- **Dirty-state handling** — stash/restore; never overwrite uncommitted work;
  surface conflicts. [Planned]
- **Pre-commit self-review** — agent reviews its own diff before committing.
  [Planned]
- **Worktree integration** — isolated worktree + branch per task/run. [Planned]

## GitHub
- **PR lifecycle** — create, update, and describe pull requests. [Planned]
- **PR review** — read diffs, post inline comments, address feedback, resolve
  threads. [Planned]
- **Issues** — issue → task → linked PR → close on merge. [Planned]
- **CI iteration** — read check status, fetch failing logs, iterate until passing.
  [Planned]
- **Stacked PRs** — dependent-PR stacks. [Planned]

## Repo awareness
- **Tree-sitter repo map** — ranked-symbol map of the codebase. [Planned]

## Provider awareness
- **Provider capability matrix** — per-model context window, cache support,
  tool-call format, reasoning controls, JSON reliability, image support. [Planned]

## Safety
- **Approval gates** — explicit confirmation for write / bash / git / GitHub
  actions. Policy unspecified. [Planned]
- **Reversible actions** — checkpoint/rollback for destructive operations.
  [Planned]
- **Secret redaction** — redact secrets from stored content and summaries.
  [Planned]
- **Subagent tool permissions** — per-worker tool allowlist. [Planned]

## Out of scope
- Pi execution modes (interactive / print-JSON / RPC over JSONL / SDK surface).
- SDK / embedding surface for building other agents.
