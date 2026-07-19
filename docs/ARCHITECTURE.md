# Iris — Architecture: Three-Tier Split

> Status (2026-06-22): target architecture, realized in the runtime. Iris
> ships today as one binary with the tier boundaries enforced in-module, and the
> async-hard agent loop (below) is shipped. This document defines the ownership
> tiers Iris is converging on, modeled on pi's
> `packages/agent` (core) / harness / `packages/coding-agent` layering. It is a
> design target, not an implementation snapshot; see
> [`CODEMAPS/INDEX.md`](CODEMAPS/INDEX.md) for what exists now and
> [`ROADMAP.md`](ROADMAP.md) for build order.

## The one rule

The agent loop **emits events and calls hooks**. It imports no UI, no approval
UX, and no concrete tool implementations. Dependencies point *inward* toward
contracts. Everything else is a consequence of this rule.

This mirrors pi's `@earendil-works/pi-agent-core`, which has zero UI dependency
and ships no tool implementations of its own — it is the engine, not the car.

Runtime hardness does **not** change this rule. The shipped async-hard loop
preserves the same dependency direction: provider stream reads and tool futures
are raced against cancellation (`tokio::select!`) inside Nexus; terminal Ctrl-C
and approval UX remain outside Nexus and enter through contracts (the CLI owns
the tokio runtime and a Ctrl-C watcher thread that trips the turn's
`CancellationToken`).

Reference split:

- `~/vendor/pi-mono` is the contract/layering reference. pi-mono is already
  async TypeScript, but its loop is intentionally linear and easy to reason
  about.
- `~/vendor/codex` is the primary Rust runtime reference. Copy the boring core
  ideas: Tokio streams, `CancellationToken`, `tokio::select!`, child tool
  cancellation, and safe-parallel/exclusive tool execution.
- `~/vendor/claude-code` validates product edge cases such as synthetic tool
  results after abort and concurrency-safe batching; do not port its TypeScript
  structure.
- `~/vendor/pi_agent_rust` is a secondary sketch only. Do not adopt `asupersync`,
  a custom runtime, or a monolithic agent file.

## The three tiers

```
╭──────────────────────────────────────────────────────────────────────╮
│ TIER 3 — Iris (CLI + adapters)      (pi: packages/coding-agent, pi-ai) │
│   terminal I/O, approval prompts, tool impls, provider/auth adapters   │
╰───────────────────────────────┬──────────────────────────────────────╯
                                │ depends on
                                ▼
╭──────────────────────────────────────────────────────────────────────╮
│ TIER 2 — Wayland (harness)          (pi: packages/agent/src/harness/)  │
│   sessions, config, path safety, output handles, compaction, skills     │
╰───────────────────────────────┬──────────────────────────────────────╯
                                │ depends on
                                ▼
╭──────────────────────────────────────────────────────────────────────╮
│ TIER 1 — Nexus (core)               (pi: packages/agent core)          │
│   model loop, contracts, event stream, tool + approval hook traits     │
│   imports NOTHING from tiers above                                     │
╰──────────────────────────────────────────────────────────────────────╯
```

### Tier 1 — Nexus (core)

The provider-neutral, UI-neutral engine. Owns the model loop, the conversation
contracts, the event stream front-ends subscribe to, and the trait seams that
tools and approval plug into.

| Owns | Today's file(s) |
|---|---|
| Model loop (tokio async: turn → provider stream → tool → repeat, bounded round-trips, per-turn `CancellationToken` raced via `tokio::select!`) | `nexus.rs` |
| Async streaming provider contract `ChatProvider::respond_stream` → `Stream<ProviderEvent>` | `nexus.rs` |
| Message contracts: `Message`, `Role`, `ToolCall`, `AssistantTurn` | `nexus.rs` |
| Agent-event stream (`AgentEvent`, `AgentObserver`) | `nexus.rs` |
| Async `Tool` trait (`execute` future + child token, `is_concurrency_safe`) + `ToolOutput`/`ToolEnv` contracts (not the implementations) | `nexus.rs` |
| Injected tool set + name lookup (`Tools::by_name`); approval-policy enforcement; sequential-default scheduling with safe-parallel batching of concurrency-safe, ungated calls | `nexus.rs` |
| `ApprovalGate` approval hook + `ApprovalDecision` (the contract — not the UX) | `nexus.rs` |
| Boundary errors, exit codes, tracing | `errors.rs`, `telemetry.rs` |

Must **not** import: anything in Tier 2 or Tier 3 (no `ui`, no `approval` UX, no
concrete `tools::*` implementations, no `session`/`config`).

pi equivalent: `agent.ts`, `agent-loop.ts`, `types.ts`, `proxy.ts`.

### Tier 2 — Wayland (harness)

The batteries-included runtime that bolts persistence and the execution
environment onto the bare core loop. In pi this is the `AgentHarness` /
`ExecutionEnv` layer; an application picks the bare-core tier or this tier.

| Owns | Today's file(s) |
|---|---|
| Harness wrapping the bare agent: owns the execution env + session, injects `ToolEnv`, persists complete round trips plus the final/error backstop | `wayland/mod.rs` (`Harness`) |
| Session transcript persistence/read store | `session.rs` |
| Settings / configuration loading, including global-only provider/base-url/scoped-model/cache/context-management controls and project-safe model/reasoning/context-budget overrides | `config.rs` |
| Workspace path safety (the FS/Shell sandbox surface) | `tools/path.rs`, `tools/bash/sandbox.rs` |
| Tool execution state (observed files, bash sessions) | `tools/observe.rs`, `tools/bash/session.rs` (`ToolState`) |
| Host capabilities, if a plugin system is ever added (`host_read`, `host_ls`, later `host_*_plan`) | _exploratory (issue #18)_ |
| Oversized tool-output handle storage | `handles.rs`, `wayland/mod.rs` |
| Context compaction policy, range planning, stale-result revalidation, safe-boundary apply, mid-turn governor, hybrid measurement, and trigger ladder | `wayland/compaction.rs`, `wayland/compaction_governor.rs`, `wayland/compaction_background.rs`, `wayland/trigger.rs`, `wayland/mod.rs`, `session.rs` |
| Iris adapters for the shared worker scheduler: `!Send` executor registration, versioned effective-route persistence, accepted-request provider construction, child Nexus loops, compaction executors, and worktree linkage | `wayland/worker_runtime.rs`, `wayland/subagents.rs`, `wayland/compaction_background.rs` |
| System-prompt / project-instruction assembly (fragments + generated tool blocks + project docs + runtime context) | `wayland/system_prompt/` |
| Skills: bounded repo/user/system/admin discovery, Codex metadata/config compatibility, metadata budgeting, contextual injection, confined resource reads, refresh-at-turn-boundary | `wayland/skills/`, `wayland/mod.rs`, `tools/read.rs` |

Depends on Tier 1 and the host-neutral `iris-subagent-runtime` support crate. That
crate is not a fourth Iris product tier: it owns reusable bounded scheduling,
durability, artifacts, groups, and worktree infrastructure and imports no Iris,
Nexus, Mimir, settings, provider, or terminal code. Wayland supplies executors
that run Nexus child loops or compaction calls; compaction retains policy and
safe-boundary application. The `Harness` is the analogue of pi's `AgentHarness`
(`agent-harness.ts`): it owns `env`/`session`, passes `env` into the run, and
appends transcript messages itself.

Direct worker routing preserves the tier split. Mimir applies optional model and
reasoning overrides to a resolved `ModelSelection` and validates the result before
worker acceptance. The Iris tool adapter snapshots provider, model, base URL, and
normalized effort. Wayland stores that versioned, non-secret route on the
host-owned `WorkerRequest` seams and passes the accepted request to
`ChildProviderFactory`; the factory constructs the `!Send` provider on the
scheduler thread. A queued worker therefore cannot follow later parent model or
reasoning changes. Route-less legacy requests retain live-parent inheritance. A
request that claims an Iris route but carries malformed metadata fails closed.

Worker type manifests supply model fallback chains, system prompts, tool profiles,
child policy, and provider-round defaults. The Iris tool adapter applies explicit
spawn overrides, clamps the resolved tool set to the parent ceiling, and persists
the final route as the execution contract. Manifests do not change Wayland
persistence or provider construction.

pi equivalent: `src/harness/` — `agent-harness.ts`, `session/`, `compaction/`,
`skills.ts`, `system-prompt.ts`, `env/nodejs.ts` (`ExecutionEnv` = `FileSystem`
+ `Shell`).

### Tier 3 — Iris (CLI + adapters)

The front-end and the concrete plug-ins. Terminal interaction, the approval
prompt UX, the actual tool implementations, and the provider/auth adapters that
translate wire formats into the Tier 1 `ChatProvider` contract.

| Owns | Today's file(s) |
|---|---|
| CLI entrypoint, command dispatch, session driver | `main.rs`, `cli.rs` |
| Terminal I/O behind the `Ui` trait | `ui/`, `tool_display.rs` |
| Render backends: screen-mode policy (ADR-0029) selects the alt-screen pager (full-frame ratatui `Terminal`) or the inline terminal surface (ADR-0006); both render the same `Screen` state | `ui/screen_mode.rs`, `ui/tui/pager.rs`, `ui/terminal_surface.rs` |
| Approval prompt UX + `ApprovalGate`/`AgentObserver` adapter (`UiBridge`); decision parsing | `ui/` (`UiBridge`, `request_approval`), `approval.rs` (`parse_decision`) |
| Tool implementations: workspace tools plus model-facing subagent spawn/status/cancel/output/plan/apply adapters | `tools/*`, `tools/registry.rs` |
| Subagent/worktree operator commands and apply authorization UX | `cli.rs`, `ui/tui_loop.rs`, `ui/slash.rs` |
| Plugin runtime + registration, if a plugin system is ever added: executor (WASM/Extism or subprocess), manifest parsing, registry wiring | _exploratory (issue #18)_ |
| Trusted approval-preview diff rendering | `tools/mod.rs` (`diff_preview`) → Tier 3 |
| Provider adapters (translate OpenAI Codex Responses, Anthropic Messages, and Antigravity/Gemini wire formats → contracts), packaged as **Mimir** (the AI/provider package; see [`NAMING.md`](NAMING.md)); provider-native prompt-cache/context-management request knobs stay inside these adapters | `mimir/providers/*` |
| Auth flows + token store (Mimir), including shared cancellable loopback OAuth callback plumbing | `mimir/auth/*` |

Depends on Tier 1 (contracts) and Tier 2 (harness).

pi equivalent: `packages/coding-agent` (tools + TUI) and `pi-ai` (the
provider-abstraction the adapters implement against).

## Current vs target

The four cuts are done: the bare `Agent` in `nexus.rs` is a provider-, UI-,
persistence-, and workspace-neutral in-memory engine. It imports no `crate::ui`/
`crate::approval` (Step A), resolves tools by name over an injected set (Step B),
and owns no filesystem or session store (Step C) -- the Tier-2 `Harness`
(`wayland/mod.rs`) owns the execution env and live persistence, while `session.rs`
also owns the read-side `SessionStore`; Wayland injects a `&ToolEnv`
into each turn. The only `crate::tools` reference left in core is the `ToolState`
type borrowed through `ToolEnv` (the type stays in `crate::tools`; the harness
owns the instance). The cuts that reached this split:

1. **Loop emits events, not UI calls.** _(done)_ The loop emits a Tier-1
   `AgentEvent` stream to an `AgentObserver`; `crate::ui` is gone from the loop.
   Tier 3 maps `AgentEvent` to `UiEvent` in `UiBridge`.
2. **Approval becomes a hook.** _(done)_ The `ApprovalGate` trait lives in core;
   the CLI supplies the prompting implementation via `UiBridge`. The loop only
   calls `gate.review(...)`, and the approval policy stays Nexus-owned.
3. **Tools become injected.** _(done)_ A `Tool` trait lives in core; Tier 3
   builds the set (`built_in_tools()`) and injects it into the agent, which
   resolves calls by name lookup over the injected `Tools` (no
   `crate::tools::dispatch` name-match). Tool impls and self-classification
   (`requires_approval`/`is_destructive`/`diff_preview`/`supports_allow_always`)
   live in Tier 3; the loop still enforces the approval policy. The thin `Tools`
   lookup is justified by modes, subagents, and provider-specific tools; a plugin
   system (issue #18) would be one optional consumer, not the reason for it.
   Relocating `ToolState` to the harness is Step C. See "Tools across the tiers".
4. **Persistence + execution surface are harness-tier.** _(done)_ The bare
   `Agent` holds no `workspace`, `ToolState`, `SessionLog`, or `SessionStore`. The Tier-2
   `Harness` (`wayland/mod.rs`) wraps the agent, owns the workspace + `ToolState`
   (injected per turn as `ToolEnv`) and the optional `SessionLog`, and persists
   complete provider round trips through an inert-by-default Nexus observer
   boundary, with a final/error diff after each turn as the backstop. The
   read-side `SessionStore` lists/opens persisted transcripts for `resume <id>`
   and compaction-aware context rebuild, still outside the core loop --
   mirroring pi's `AgentHarness` owning `ExecutionEnv`
   + session and appending messages itself, never in Nexus.

## Tools across the tiers

Tools are not one tier. Today `src/tools/` is a cross-tier bundle; the split
slices it three ways. The registry refactor that implements this slicing is
driven by modes, subagents, and provider-specific tools; a plugin system
(issue #18) is only an optional future consumer of the same seam, not the reason
for it.

| Concern | Tier | Notes |
|---|---|---|
| `Tool` trait, `ToolOutput`, `ToolOutputStore`, `Tools` registry/surface planner, approval **enforcement** | 1 Nexus | Core names no concrete tool and knows nothing about any plugin runtime. Tools classify themselves (`requires_approval`, `is_destructive`, `supports_allow_always`, `is_concurrency_safe`); core enforces. |
| Workspace path safety, `ToolState`, session-scoped `HandleStore`, host capabilities (`host_read`/`host_ls`) | 2 Wayland | The execution surface (`env`) passed to `Tool::execute`. Plugins get host functions, never raw WASI. |
| Built-in impls (`read`..`ls`), registry construction, trusted diff rendering, and — only if a plugin system is added — a plugin executor + manifest parsing | 3 Iris | Concrete impls + wiring. The diff renderer is host-side and trusted relative to any plugin. |

Runtime completion added one more tool contract without changing ownership:
tools declare whether a call is concurrency-safe (`Tool::is_concurrency_safe`,
default `false` = exclusive). Nexus enforces the scheduling rule; each concrete
tool owns its classification. Today `grep`/`find`/`ls` are concurrency-safe;
file-mutating tools, shell commands, and `read` (it mutates `ToolState`) stay
exclusive unless a future measured case proves a narrower safe mode.

Two boundaries are orthogonal and must not be conflated:

- **Ownership tier** (Nexus / Wayland / Iris) — the dependency-direction split
  in this document.
- **Trust boundary** (trusted host vs untrusted plugin) — a plugin is untrusted
  regardless of which tier loads it. The trusted approval-preview renderer
  therefore lives at Tier 3 and is still "trusted" because it is host code, not
  plugin-supplied.

Classification vs enforcement: the *decision* to gate a mutating tool, re-prompt
a destructive call, or refuse bash-always stays in Nexus (Tier 1) per
`AGENTS.md`. The *knowledge* of whether a given tool mutates or a given bash
command is destructive rides with the tool as `Tool`-trait methods (Tier 3), so
core never matches on tool names.

Provider-native optimization follows the same ownership rule. Nexus carries only
provider-neutral messages, opaque compaction values, continuity strings,
provider usage metadata, a capability enum, and typed events. Mimir decides
whether OpenAI receives prompt-cache keys or 24h retention, whether Anthropic
receives `cache_control` or supported `context_management` edits, whether an
opaque compaction block is replayable for the exact selection, and whether
Gemini tool calls need a `thoughtSignature` echoed back. Wayland owns the single
plan/revalidate/persist/apply path; a native provider is only another background
summarizer and never mutates context directly. Every native entry also has a
portable text summary, so model/provider switches and resume do not depend on an
opaque block. User-controlled knobs for these behaviors are global settings
because they can affect privacy, cost, or provider routing; repo settings cannot
enable them.

## Packaging

Per the project agent guidelines, these tiers are **modules in one crate** for
the MVP. Do not split into separate crates or processes to satisfy the boundary. The
discipline is the inward-pointing dependency direction, not the package count.
Promote `nexus` (core), `wayland` (harness), and `iris` (CLI) to a cargo
workspace only when a second front-end or published Nexus runtime makes the
split pay for itself; once the imports point inward, that promotion is
mechanical.
