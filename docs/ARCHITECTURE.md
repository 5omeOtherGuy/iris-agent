# Iris — Architecture: Three-Tier Split

> Status (2026-06-16): target architecture. Iris ships today as one binary with
> the tier boundaries only partially enforced (see "Current vs target"). This
> document defines the ownership tiers Iris is converging on, modeled on pi's
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
│   sessions, config, workspace/path safety, (later) compaction, skills  │
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
| Model loop (turn → provider → tool → repeat, bounded round-trips) | `nexus.rs` |
| Provider contract `ChatProvider` | `nexus.rs` |
| Message contracts: `Message`, `Role`, `ToolCall`, `AssistantTurn` | `nexus.rs` |
| Agent-event stream + sink (`AgentEvent`, `AgentObserver`, `TurnSink` for deltas) | `nexus.rs` |
| `Tool` trait (the contract — not the implementations) | _target: new in core_ |
| `ToolRegistry` / `ToolPolicy` (registration, dispatch order, identity, approval enforcement) | _target: new in core_ |
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
| Session transcript persistence | `session.rs` |
| Settings / configuration loading | `config.rs` |
| Workspace path safety (the FS/Shell sandbox surface) | `tools/path.rs`, `tools/bash/sandbox.rs` |
| Tool execution state (observed files, bash sessions) | `tools/observe.rs`, `tools/bash/session.rs` (`ToolState`) |
| Host capabilities, if a plugin system is ever added (`host_read`, `host_ls`, later `host_*_plan`) | _exploratory (issue #18)_ |
| Context compaction | _planned_ |
| Skills / system-prompt assembly | _planned_ |

Depends on Tier 1 only.

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
| Approval prompt UX + `ApprovalGate`/`AgentObserver` adapter (`UiBridge`); decision parsing | `ui/` (`UiBridge`, `request_approval`), `approval.rs` (`parse_decision`) |
| Tool implementations: `read` `write` `edit` `bash` `grep` `find` `ls` | `tools/*` (impls) |
| Plugin runtime + registration, if a plugin system is ever added: executor (WASM/Extism or subprocess), manifest parsing, registry wiring | _exploratory (issue #18)_ |
| Trusted approval-preview diff rendering | `tools/mod.rs` (`diff_preview`) → Tier 3 |
| Provider adapter (translates Codex Responses → contracts) | `providers/*` |
| Auth flows + token store | `auth/*` |

Depends on Tier 1 (contracts) and Tier 2 (harness).

pi equivalent: `packages/coding-agent` (tools + TUI) and `pi-ai` (the
provider-abstraction the adapters implement against).

## Current vs target

The tiers above are an ownership target. The loop in `nexus.rs` no longer
imports `crate::ui` or `crate::approval` (Step A, done), but it still calls
`crate::tools::{dispatch, requires_approval, is_destructive, diff_preview}`
directly. Four cuts invert those dependencies to reach the split:

1. **Loop emits events, not UI calls.** _(done)_ The loop emits a Tier-1
   `AgentEvent` stream to an `AgentObserver`; `crate::ui` is gone from the loop.
   Tier 3 maps `AgentEvent` to `UiEvent` in `UiBridge`.
2. **Approval becomes a hook.** _(done)_ The `ApprovalGate` trait lives in core;
   the CLI supplies the prompting implementation via `UiBridge`. The loop only
   calls `gate.review(...)`, and the approval policy stays Nexus-owned.
3. **Tools become injected.** Define a `Tool` trait and a `ToolRegistry` in
   core; build the registry at Tier 3 and inject it into the agent instead of
   the hardcoded `crate::tools::dispatch` name-match. Tool impls move to Tier 3.
   This registry is justified by modes, subagents, and provider-specific tools on
   its own; a plugin system (issue #18) would be one optional consumer of the
   same seam, not the reason for it. See "Tools across the tiers" below.
4. **Persistence is harness-tier.** Keep `SessionLog` out of the bare core loop;
   it belongs to the Tier 2 harness (or a Tier 1 event subscriber).

## Tools across the tiers

Tools are not one tier. Today `src/tools/` is a cross-tier bundle; the split
slices it three ways. The registry refactor that implements this slicing is
driven by modes, subagents, and provider-specific tools; a plugin system
(issue #18) is only an optional future consumer of the same seam, not the reason
for it.

| Concern | Tier | Notes |
|---|---|---|
| `Tool` trait, `ToolOutput`, `ToolRegistry`, `ToolPolicy`, identity keys, dispatch order, approval **enforcement** | 1 Nexus | Core names no concrete tool and knows nothing about any plugin runtime. Tools classify themselves (`mutates()`, `classify(args)`); core enforces. |
| Workspace path safety, `ToolState`, host capabilities (`host_read`/`host_ls`) | 2 Wayland | The execution surface (`env`) passed to `Tool::execute`. Plugins get host functions, never raw WASI. |
| Built-in impls (`read`..`ls`), registry construction, trusted diff rendering, and — only if a plugin system is added — a plugin executor + manifest parsing | 3 Iris | Concrete impls + wiring. The diff renderer is host-side and trusted relative to any plugin. |

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

## Packaging

Per [`AGENTS.md`](../AGENTS.md), these tiers are **modules in one crate** for the
MVP. Do not split into separate crates or processes to satisfy the boundary. The
discipline is the inward-pointing dependency direction, not the package count.
Promote `nexus` (core), `wayland` (harness), and `iris` (CLI) to a cargo
workspace only when a second front-end or published Nexus runtime makes the
split pay for itself; once the imports point inward, that promotion is
mechanical.
