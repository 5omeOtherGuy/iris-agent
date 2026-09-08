# Architecture

The Iris agent has three internal tiers. Its Cargo workspace also contains
`iris-bench` and the host-neutral `iris-subagent-runtime` crate. The core loop
emits events and calls hooks; UI and session storage remain outside Nexus.
Concrete state/path helper dependencies remain documented exceptions to the
inward-dependency target, not evidence of a fully isolated core crate.

## Tiers

| Tier | Name | Owns |
| --- | --- | --- |
| 1 | Nexus | Provider-neutral model loop, messages, tool contracts, approval hook, event stream, tool scheduling, approval enforcement. |
| 2 | Wayland | Harness, sessions, settings, workspace execution surface, compaction, output handles, permission policy, task checkpoints. |
| 3 | Iris | CLI, terminal UI, approval UX, built-in tool implementations, provider/auth adapters. |

Mimir is the provider package. It implements the Nexus `ChatProvider` contract
for OpenAI Codex Responses, OpenAI API, OpenAI-compatible chat endpoints,
Anthropic Messages, and Antigravity/Gemini Code Assist.

## Core rule

Nexus must not import UI code, concrete tool implementations, session storage,
configuration loading, or provider-specific transport details. It owns the
policy decisions around tool scheduling and approval enforcement, while higher
tiers provide the concrete hooks and implementations.

Current exceptions in `src/nexus.rs` are `ToolEnv`'s `crate::tools::ToolState`,
`crate::tools::path::workspace_relative`, and
`crate::display_path::workspace_path`. See
[Current vs target](../docs/ARCHITECTURE.md#current-vs-target).

## Runtime loop

1. The CLI resolves settings, credentials, provider/model selection, tool
   registry, system prompt, project policy, and session state.
2. Wayland wraps a Nexus `Agent` in a `Harness`.
3. A user turn enters the harness.
4. Nexus calls the selected provider stream.
5. Provider deltas and final assistant turns become `AgentEvent`s.
6. Tool calls are approved or denied, then executed through injected tool
   implementations.
7. The loop repeats until the assistant stops calling tools or the configured
   soft cap is reached.
8. Wayland persists committed transcript messages and compaction metadata.

Each turn owns a cancellation token. Provider stream reads, tool futures, and
approval reviews are raced against that token. Cancelled tool calls still receive
synthetic tool results so the next provider request remains valid.

Tools run sequentially unless a concrete tool marks itself concurrency-safe.
Nexus enforces the schedule; tool implementations provide the classification.

## Module map

- `src/main.rs`: thin binary shim.
- `src/lib.rs`: process entrypoint, command dispatch, login/update commands,
  provider construction.
- `src/cli.rs`: session driver, text/TUI selection, shared slash-command
  handling.
- `src/nexus.rs`: provider-neutral agent loop and contracts.
- `src/wayland/mod.rs`: harness, session integration, compaction, output store.
- `src/wayland/git_safety/`: dirty-tree task ownership, checkpoint, rollback,
  final diff settlement.
- `src/wayland/trust.rs`: per-project permission policy store.
- `src/wayland/subagents.rs`, `src/wayland/worker_runtime.rs`: manifest-driven
  worker integration and shared scheduling adapters.
- `crates/iris-subagent-runtime/`: durable scheduling, artifacts, managed
  worktrees, recovery and reviewed apply.
- `src/harness.rs`, `iris-bench/`: benchmark facade and separate executable.
- `src/wayland/system_prompt/`: prompt assembly from internal fragments and
  project docs.
- `src/mimir/providers/`: provider adapters.
- `src/mimir/auth/`: OAuth and credential storage.
- `src/tools/`: built-in tool implementations and path safety helpers.
- `src/ui/`: TUI, text fallback, rendering, selectors, approval bridge.

## Boundaries to preserve

- No terminal/UI logic in Nexus.
- No provider-specific names, auth, endpoints, or wire schemas in Nexus.
- No approval enforcement moved out of Nexus into only the CLI.
- No workspace/session persistence moved into Nexus.
- No bespoke runtime replacing the shipped tokio async loop.
