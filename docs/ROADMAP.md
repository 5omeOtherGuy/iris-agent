# Iris — Roadmap

> Status (2026-06-15): roadmap for an early implementation. A text-only session
> loop, OpenAI Codex Responses provider, streaming tool-call loop, workspace-scoped
> tools, and terminal approval gates with diff previews exist, but the Agent
> Kernel MVP is not complete.
> This roadmap defines build order and acceptance criteria. `FEATURES.md` remains
> the capability inventory; this document says what to build first.

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

The immediate goal is much smaller: build the minimum working agent kernel.

## Current implementation snapshot

Implemented today:

- CLI entrypoint that starts Iris from `cargo run`.
- Text-only Nexus session loop with in-memory conversation state, `/exit` /
  `/quit`, and provider-error recovery, driven through a `Ui` front-end seam
  (`src/ui/`, `src/cli.rs`).
- Incremental terminal streaming of assistant text via the `TurnSink` seam and
  `UiEvent` deltas.
- Provider-neutral `ChatProvider`, `TurnSink`, `AssistantTurn`, `ToolCall`,
  `Message`, and `Role` types.
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
- OpenAI Codex OAuth token loading/refresh from the Iris auth-file shape.
- OpenAI Codex browser and device-code login flows.
- OpenAI Codex Responses request/response handling, including tool schemas and
  streamed-response parsing.
- Unit tests for the REPL, tool loop, approvals, tool implementations, path
  safety, atomic writes, auth-file handling, URL/request shaping, and response
  parsing.

Not implemented yet:

- Transcript persistence, persistent approval policies, shared
  file-observation/stale-file guards, modes, subagents, context ledger, content
  handles, git automation, and GitHub integration.

## Tool quality — best-in-class on all tools

Cross-cutting workstream, not a milestone. Goal: every built-in tool is at or
above the field's best implementation. The seven tools were assessed 2026-06-15
against Claude Code, Codex CLI, Aider, OpenHands/SWE-agent, Cline/Roo, Gemini
CLI, and oh-my-pi (omp). Each tool has a tracking issue.

| Tool | Tier today | Best-in-class holder | Issue |
| --- | --- | --- | --- |
| `edit` | Claude-compatible exact-string + replace_all + atomic writes | RooCode (fuzzy) / Codex (V4A) | [#4](https://github.com/5omeOtherGuy/iris-agent/issues/4) |
| `grep` | Standard | Claude Code / omp | [#6](https://github.com/5omeOtherGuy/iris-agent/issues/6) |
| `write` | Standard + atomic writes | Claude Code | [#5](https://github.com/5omeOtherGuy/iris-agent/issues/5) |
| `ls` | Standard | commoditized | [#8](https://github.com/5omeOtherGuy/iris-agent/issues/8) |
| `read` | Standard text read | Claude Code (multimodal) | [#2](https://github.com/5omeOtherGuy/iris-agent/issues/2) |
| `find` | Standard, wraps `fd` | Claude Code Glob (native) | [#7](https://github.com/5omeOtherGuy/iris-agent/issues/7) |
| `bash` | Behind | Claude Code / Codex | [#3](https://github.com/5omeOtherGuy/iris-agent/issues/3) |

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
   - `bash`: kernel sandbox (Landlock/Seatbelt) + persistent session +
     background jobs — the single largest gap — [#3](https://github.com/5omeOtherGuy/iris-agent/issues/3).
     Includes child-process-group lifecycle: a `bash` child runs in its own
     process group, so the second-Ctrl-C force-quit (#9) currently kills the
     agent without reaping a long-running child, orphaning it. Reaping/killpg
     on force-quit belongs with this work.
   - `find`/`grep`: keep wrapping `fd`/`rg`. These are the same engines the
     mature tools (Pi, etc.) wrap rather than reimplement; matching their
     behavior natively means an ongoing parity burden (smart-case, glob
     semantics, parallel walk, binary detection) for no real gain. `rg`/`fd`
     are accepted runtime dependencies. The open work is packaging, not a
     rewrite: a clear "not installed" error today, and optionally an on-demand
     download manager later (Pi's approach) —
     [#7](https://github.com/5omeOtherGuy/iris-agent/issues/7),
     [#6](https://github.com/5omeOtherGuy/iris-agent/issues/6).
3. **Tier 3 — parity polish.** `edit` `replace_all` + helpful failure output
   ([#4](https://github.com/5omeOtherGuy/iris-agent/issues/4)); `write` freshness
   guard ([#5](https://github.com/5omeOtherGuy/iris-agent/issues/5)); `read`
   PDF/notebook ([#2](https://github.com/5omeOtherGuy/iris-agent/issues/2)); `ls`
   tree view ([#8](https://github.com/5omeOtherGuy/iris-agent/issues/8)).
4. **Tier 4 — extend the frontier.** New tools beyond the seven —
   `apply_patch` (V4A; tracked under provider-specific tools,
   [#10](https://github.com/5omeOtherGuy/iris-agent/issues/10)), `ast_edit` for
   structural moves, and an optional fast-apply path.

Shared tool infrastructure issues opened 2026-06-15:

| Issue | Area | Current status |
| --- | --- | --- |
| [#11](https://github.com/5omeOtherGuy/iris-agent/issues/11) | Path identity and file observation store | Missing beyond workspace path resolution. |
| [#12](https://github.com/5omeOtherGuy/iris-agent/issues/12) | Mutation preflight and stale-file detection | Missing; no read-before-edit or stale-file guard yet. |
| [#13](https://github.com/5omeOtherGuy/iris-agent/issues/13) | Atomic file mutation layer | Partial: same-directory atomic replacement helper exists; no canonical mutation queue or observation refresh. |
| [#14](https://github.com/5omeOtherGuy/iris-agent/issues/14) | Diff/preview and approval policy | Partial: `y`/`yes` approval/deny is enforced in Nexus and file-mutating tools show a diff preview before approval; no persistent allow policies or risk labels. |
| [#15](https://github.com/5omeOtherGuy/iris-agent/issues/15) | Tool output/result/error contract | Missing structured metadata; plain text results remain. |

Status: strong-standard on the read/grep/edit/write/ls cluster, with `edit` now
on Claude Code's exact-string contract. The honest gaps are `bash` (large), shared
observation/preflight/result metadata (medium), persistent approval policy/risk
labels (medium; diff preview already shipped), and `rg`/`fd` packaging
(clear missing-binary errors, optional on-demand download; small).

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

- Better terminal UX for tool approvals and results.
- Streaming output if not already in MVP. [Shipped: `TurnSink` deltas.]
- Session transcript persistence.
- Focused config file for provider/model/tool policy.
- Safer `bash` policy. Command classification is optional and should not block
  the basic local coding workflow.
- Better `edit` semantics: uniqueness checks, conflict messages, preview diff.
  [Preview diff shipped for mutating tools; uniqueness/conflict messaging remains.]
  See the Tool quality workstream ([#4](https://github.com/5omeOtherGuy/iris-agent/issues/4)).
- Basic git diff display after file changes.
- Optional self-review before final response.

Acceptance signal: Iris can make a small code/doc change in a real repository,
show the diff, and explain what it changed.

Gate before Milestone 2: tool results must already support structured metadata, so
large outputs can later become handle-backed without changing every caller.

## Milestone 2 — Token-Efficiency Proof

**Goal:** prove the first unique Iris thesis with measurement.

Potential scope:

- Content-addressed store.
- Handle-returning large tool outputs.
- Micro-summary schema for large results.
- Selective handle dereferencing.
- Token accounting per turn.
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

Acceptance signal: Iris can safely complete a local coding task, show the diff,
and either roll it back or prepare it for commit without touching unrelated user
changes.

Gate before Git automation: dirty-tree behavior, rollback semantics, and approval
requirements must be specified before auto-commit, worktree, GitHub, or CI features
are implemented.

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
3. Continue Milestone 1 UX work: streaming output and diff/tool-result
   presentation are shipped; transcript persistence and a provider/model/tool
   config file remain.
4. Implement shared tool infrastructure in dependency order: path identity and
   observation store ([#11](https://github.com/5omeOtherGuy/iris-agent/issues/11)),
   mutation preflight ([#12](https://github.com/5omeOtherGuy/iris-agent/issues/12)),
   atomic queue/refresh completion ([#13](https://github.com/5omeOtherGuy/iris-agent/issues/13)),
   approval/diff UX ([#14](https://github.com/5omeOtherGuy/iris-agent/issues/14)),
   and result metadata ([#15](https://github.com/5omeOtherGuy/iris-agent/issues/15)).

## Implementation notes backlog

These topics are intentionally not specified in this roadmap, but should be
resolved in focused implementation notes before the relevant milestone starts:

- `NEXUS_MVP_DESIGN.md` — Nexus/Iris CLI boundaries, provider-neutral messages,
  provider interface, tool registry, and approval policy.
- `TOOL_CONTRACTS.md` — input schemas, result/error format, and per-tool behavior
  for `read`, `write`, `edit`, and `bash`.
- `SAFETY_MODEL.md` — workspace path safety, shell limits, approval gates,
  destructive-action policy, and secret handling.
- `BENCHMARK_PLAN.md` — token/cost/latency/task-success measurements for the
  handle-based token-efficiency proof.
