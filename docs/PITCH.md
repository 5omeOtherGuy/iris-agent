# Iris

**A fast, token-efficient coding agent for your terminal.**

> **Status (2026-06-22): Milestone 2 foundations are implemented.** Iris
> currently has a terminal-surface TUI with Iris-owned transcript replay plus a
> text fallback, selectable Mimir providers (`openai-codex`, `anthropic`,
> `antigravity`), runtime model/reasoning switching, streamed response parsing,
> workspace-scoped built-in tools, approval gates with diff previews,
> fragment-based system-prompt assembly, provider/model/reasoning/context-budget
> settings, linear session resume, JSONL transcripts, session-scoped large-output
> handles, token estimates, turn-boundary auto-compaction, and default-off
> provider-native prompt-cache/context-management controls. Nexus runs a
> tokio async loop with turn-level cancellation: async provider streams,
> per-tool child cancellation, and safe-parallel execution of concurrency-safe
> tools. The active milestone gate is proving the token/context foundations with
> measurement.
> Efficiency claims (token savings, cache hits, cheaper switching, compaction
> quality) are **design goals to be backed by benchmarks**
> before they are used as selling points. Read future capability sections below
> as *"designed to,"* not *"does."* Per-capability status and MVP scope live in
> [`FEATURES.md`](FEATURES.md).

Iris is being built as a coding agent you run in your terminal — the tool you
reach for to read, edit, and ship code, not a library you build other agents on
top of. It is written in Rust for a small, fast, single-binary footprint, but
speed and a small binary are table-stakes today, not the reason to use it.

The product naming is intentionally split:

- **Iris** is the coding agent — the product users experience.
- **Nexus** is the agent runtime core — the engine that runs model loops, tools,
  context, safety, and execution.
- **Iris CLI** is the terminal interface to Iris and Nexus.

The reason to build Iris is one conviction carried down to the runtime: **every
token should be deliberate.** Not truncated when the window fills, not dumped in
blindly and summarized in a panic — *budgeted, justified, cached, and
freshness-checked* before it reaches a model. The intended payoff is felt by the
person at the keyboard: **lower cost per session, faster turns, more useful
context before the agent loses the thread, and a workflow built around the diff
you ship** rather than the chat transcript. Those are the goals the build is
aimed at — to be measured, not assumed.

Put another way: *Iris aims to be a token-aware coding agent that minimizes wasted
context through explicit budgeting, typed content handles, cache-aware prompt
assembly, and budgeted delegation.*

## The token & context engine

The core of Iris is a context engine designed to decide, before every model call,
exactly what earns a place in the prompt. A **token budget planner** is meant to
allocate the window across system prompt, tools, history, files, summaries, and
the current task — explicitly, not by best-effort truncation. A **context ledger**
records *why* each piece is present (your request, the active file, a recent tool
result, a compacted summary, pinned memory), so inclusion is auditable and
evictable by reason rather than guesswork.

Large content is designed not to be copied around blindly. The first slice is
implemented for oversized tool outputs: successful results over the inline
threshold are stored beside the session transcript behind stable handles, while
the model sees a compact preview plus structured metadata. The broader
**content-addressed store** direction still includes files, web pages, diffs, and
summaries by hash, with the model dereferencing full bytes only when it actually
needs to reason over them. For coding work, context is intended to be
**diff-aware**: git diffs, touched files, nearby symbols, and recent edits over
whole-file dumps.

Prompts are to be assembled from **reusable segments** — base instructions, tool
descriptions, repo context, mode rules, provider hints — laid out **cache-aware**
so the stable parts line up with what each provider rewards for cache hits, with
hit/miss and cost-avoided surfaced so the savings are visible. When context must
shrink, the aim is to do it well: layered compaction with **freshness rules** so a
summary made before a file changed isn't trusted blindly, and **verification
probes so compaction quality can be measured rather than asserted.** The current
runtime has a deterministic turn-boundary auto-compaction foundation plus
default-off public prompt-cache hints for OpenAI/Anthropic and Anthropic
clear-edit context-management opt-ins; quality summaries, provider compact replay,
and compaction benchmarks are still future work.

## Modes that switch cheaply

Iris is designed around **modes** in the pi-mmr sense: switching between models and
thinking/effort levels mid-session. The aim is to make switching *cheap* — reuse
the already-assembled context and change only the mode-specific prompt and tool
segments, keeping the stable prefix cache-hit where the provider and model allow
it. (A cross-model or cross-provider switch generally can't reuse the provider
cache, and context still has to be sent — the goal is to minimize the overhead, not
to pretend it's free.) Over time, **mode profiles** are meant to become
deterministic: a mode defines prompt shape, tool set, compaction policy, and model
preference, not just "which model to call."

The same discipline is meant to apply to tools: a **dynamic tool surface** exposing
only the tools relevant to the current mode and task, with **compact, strict
schemas**. A **provider capability matrix** is to track each model's context
window, cache support, tool-call format, reasoning controls, and reliability, so
layout, budgeting, and model selection are informed rather than hard-coded.

## Subagents as tools

Iris is designed to treat subagents as **tools the main agent calls directly**: a
fast codebase searcher, an advisor for review and hard reasoning, a researcher for
remote repositories, a bounded task worker, and your own custom subagents. Each is
meant to run on **its own model, thinking level, and tool set**, so the main thread
can stay on a strong model while search runs on a cheap fast one and the advisor on
a high-reasoning one — multi-model routing folded into delegation.

A worker is meant to run a **fresh, isolated conversation** — that isolation is
usually the point, keeping the main thread clean while the worker does its job and
hands back a result. A worker returns a **handle + short summary** rather than
dumping its full output into the main thread. When the agent is just *coordinating*
— handing one worker's result to the next, or storing it — those bytes need not
enter the main context at all; when it needs to *reason* over them, it dereferences
the handle and pays for what it reads. The intent is that large worker outputs stop
bloating the transcript and workers run under real token and turn budgets.

This pattern is proven — pi-mmr ships it today — but pi-mmr runs as a guest inside
another agent's runtime, so its workers spawn as separate processes; sharing a
content store or enforcing a live budget across processes is possible but more
awkward and higher-overhead. Because Iris would own its runtime, workers can run
**in-process** over one shared store, which makes handle-passing and live budgets
simpler and more centrally controlled. The point isn't startup speed (negligible
next to model latency) — it's the capabilities the shared runtime is meant to
unlock.

## Keeping the door open for plugins

Iris's built-in tools stay native and trusted, and the product is **not** being
built around a plugin system. But the tool surface is designed so one could be
added later without re-plumbing the core: the Nexus-owned `ToolRegistry` and
`Tool` contract — needed anyway for modes, subagents, and provider-specific
tools — is the natural seam a plugin would plug into. If third-party extensions
ever earn their keep, the likely shape is sandboxed plugins with no raw workspace
access, explicit host capabilities that reuse the core's path-safety, and
approval keyed on plugin identity. WASM (Extism on Wasmtime) is one candidate
backend and a subprocess protocol is another; the choice is open. This is a
possibility we are deliberately keeping open, tracked in issue #18 — not a
commitment, and nothing the rest of the build depends on.

## Edits that don't waste output tokens

Iris is exploring **content-hash anchored edits**: the model points at stable
anchors in a file instead of retyping surrounding lines, so large edits stop
re-emitting code the model already saw — cutting *output* tokens, where cost and
latency pile up on big changes. (Early direction; the anchor format and fallbacks
still need specifying.)

## The diff is the deliverable

A coding agent is judged on the **diffs, commits, and PRs it ships** — not chat
quality — and Iris's workflow is being built around that. Near term: solid local
git ergonomics — a clean **diff** for every change and **checkpoint/rollback** so
you can undo a whole task, not just a stray line.

The longer-term direction is a full diff-centered workflow — auto-commits with
sensible messages, per-hunk staging, worktrees for parallel runs, and GitHub
integration that opens and reviews PRs and iterates on failing CI until the build
is green. That is roadmap, intended to ship behind a strict safety model (explicit
approval points, reversible actions, scoped permissions) and after the core engine
is proven, not before.

## Provider-agnostic by design

Iris is being built provider-agnostic from the core, with provider-specific
optimizations where they matter — cache layout, tool-call formats, reasoning
controls. Today it ships OpenAI Codex Responses, Anthropic Messages on the Claude
Code OAuth lane, and Antigravity/Gemini Code Assist, including provider-specific
continuity requirements such as Anthropic signed/redacted thinking and Gemini
tool-call `thoughtSignature` replay. The capability matrix is still planned so
each backend can become a first-class citizen rather than a
lowest-common-denominator adapter.

## Why Iris, and what it is not

The field is crowded. Lean Rust harnesses, automatic compaction, single static
binaries, and multi-provider support already exist — even in small clones — so Iris
does **not** sell raw speed or "we have compaction." Those are the floor.

Iris's bet is that the next gains come from *how deliberately an agent spends
tokens and how cleanly it fits a real coding workflow*: a context engine where
every token is justified, modes that switch cheaply, edits and tool schemas
designed to minimize output, compaction good enough to measure, and a workflow
where the diff — not the conversation — is the product. That bet is a hypothesis to
be proven with benchmarks, not a claim of present superiority.

In short: **Iris aims to be a fast, token-efficient coding agent for the terminal —
where every token is accounted for and the pull request is the deliverable.**
