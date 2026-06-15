# Iris — Roadmap

> Status (2026-06-15): roadmap for an early implementation. A text-only REPL,
> OpenAI Codex Responses provider, tool-call loop, and workspace-scoped tools
> exist, but the Agent Kernel MVP is not complete.
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
- Text-only Nexus REPL with in-memory conversation state, `/exit` / `/quit`, and
  provider-error recovery.
- Provider-neutral `ChatProvider`, `AssistantTurn`, `ToolCall`, `Message`, and
  `Role` types.
- Provider tool-call loop with bounded iterations and structured tool
  result/error messages.
- Workspace-scoped built-in tools: `read`, `write`, `edit`, `bash`, `grep`,
  `find`, `ls`, and `hashline_edit`.
- Workspace path-safety enforcement for existing and newly written paths.
- OpenAI Codex OAuth token loading/refresh from the Iris auth-file shape.
- OpenAI Codex browser and device-code login flows.
- OpenAI Codex Responses request/response handling, including tool schemas and
  streamed-response parsing.
- Unit tests for the REPL, tool loop, tool implementations, path safety,
  auth-file handling, URL/request shaping, and response parsing.

Not implemented yet:

- Approval prompts and denied-call handling.
- Incremental terminal streaming, transcript persistence, modes, subagents,
  context ledger, content handles, git automation, and GitHub integration.

## Tool quality — best-in-class on all tools

Cross-cutting workstream, not a milestone. Goal: every built-in tool is at or
above the field's best implementation. The eight tools were assessed 2026-06-15
against Claude Code, Codex CLI, Aider, OpenHands/SWE-agent, Cline/Roo, Gemini
CLI, and oh-my-pi (omp). Each tool has a tracking issue.

| Tool | Tier today | Best-in-class holder | Issue |
| --- | --- | --- | --- |
| `hashline_edit` | Frontier (parity w/ omp) | omp (origin) | [#9](https://github.com/5omeOtherGuy/iris-agent/issues/9) |
| `edit` | Strong-standard | RooCode (fuzzy) / Codex (V4A) | [#4](https://github.com/5omeOtherGuy/iris-agent/issues/4) |
| `grep` | Standard | Claude Code / omp | [#6](https://github.com/5omeOtherGuy/iris-agent/issues/6) |
| `write` | Standard | Claude Code | [#5](https://github.com/5omeOtherGuy/iris-agent/issues/5) |
| `ls` | Standard | commoditized | [#8](https://github.com/5omeOtherGuy/iris-agent/issues/8) |
| `read` | Standard + false claim | Claude Code (multimodal) | [#2](https://github.com/5omeOtherGuy/iris-agent/issues/2) |
| `find` | Standard, weak packaging | Claude Code Glob (native) | [#7](https://github.com/5omeOtherGuy/iris-agent/issues/7) |
| `bash` | Behind | Claude Code / Codex | [#3](https://github.com/5omeOtherGuy/iris-agent/issues/3) |

Execution order (by impact/effort, independent of the milestone sequence):

1. **Tier 1 — correctness bugs (ship first).** Both advertise a capability the
   code does not deliver.
   - `read`: image attachments are described but `read()` returns
     `from_utf8_lossy`, garbling images — [#2](https://github.com/5omeOtherGuy/iris-agent/issues/2).
   - `grep`: `hashline` is advertised but `GrepInput` has no such field and no
     tags are emitted — [#6](https://github.com/5omeOtherGuy/iris-agent/issues/6).
2. **Tier 2 — close real capability gaps.**
   - `bash`: kernel sandbox (Landlock/Seatbelt) + persistent session +
     background jobs — the single largest gap — [#3](https://github.com/5omeOtherGuy/iris-agent/issues/3).
   - `find`/`grep`: go native (`ignore` + `globset`, `grep-searcher`) to drop the
     `fd`/`rg` runtime deps and honor the single-static-binary pitch —
     [#7](https://github.com/5omeOtherGuy/iris-agent/issues/7), [#6](https://github.com/5omeOtherGuy/iris-agent/issues/6).
3. **Tier 3 — parity polish.** `edit` `replace_all` + helpful failure output
   ([#4](https://github.com/5omeOtherGuy/iris-agent/issues/4)); `write` freshness
   guard ([#5](https://github.com/5omeOtherGuy/iris-agent/issues/5)); `read`
   PDF/notebook ([#2](https://github.com/5omeOtherGuy/iris-agent/issues/2)); `ls`
   tree view ([#8](https://github.com/5omeOtherGuy/iris-agent/issues/8)).
4. **Tier 4 — extend the frontier.** Integrate + benchmark `hashline_edit`
   ([#9](https://github.com/5omeOtherGuy/iris-agent/issues/9)); then new tools
   beyond the eight — `apply_patch` (V4A; tracked under provider-specific tools,
   [#10](https://github.com/5omeOtherGuy/iris-agent/issues/10)), `ast_edit` for
   structural moves, and an optional fast-apply path.

Status: already best-in-class on `hashline_edit`; strong-standard on the
read/grep/edit/write/ls cluster. The honest gaps are `bash` (large), two
false-advertising bugs (small), and native search/find packaging (medium).

## Provider-specific tools

Complementary long-term axis to the tool-quality work above, tracked as an epic
([#10](https://github.com/5omeOtherGuy/iris-agent/issues/10)). The eight tools
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
  - Require confirmation before `write`, `edit`, and `bash`, unless explicitly run
    in an unsafe/non-interactive mode later.
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
4. Create a new file through the `write` tool after confirmation. [Tool
   implemented; approval pending]
5. Modify an existing file through the `edit` tool after confirmation. [Tool
   implemented; approval pending]
6. Run a harmless shell command through the `bash` tool after confirmation. [Tool
   implemented; approval pending]
7. Return tool errors to the model without crashing. [Implemented]
8. Keep all file operations inside the workspace by default. [Implemented]
9. Exit cleanly without corrupting session state or files. [Partial]

### Remaining MVP design decisions

These should be specified in focused implementation notes rather than expanded
here:

- Approval policy for `write`, `edit`, and `bash`, including denied calls.
- Whether destructive or externally visible `bash` commands need classification
  beyond the baseline approval gate.
- Whether additional file-tool policy is needed for binary files, very large
  files, and absolute paths before marking path safety complete.

### MVP verification gate

Before Milestone 0 is considered complete, verification should include:

- Unit coverage for workspace path safety and edit behavior.
- Unit coverage for tool result/error encoding.
- A fake-provider integration test covering prompt → tool call → tool result →
  final assistant response.
- A manual smoke test with one real provider.

## Milestone 1 — Usable Local Coding Agent

**Goal:** make the kernel pleasant and safe enough for routine local coding tasks.

Potential scope:

- Better terminal UX for tool approvals and results.
- Streaming output if not already in MVP.
- Session transcript persistence.
- Focused config file for provider/model/tool policy.
- Safer `bash` policy. Command classification is optional and should not block
  the basic local coding workflow.
- Better `edit` semantics: uniqueness checks, conflict messages, preview diff.
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

1. Implement the Agent Kernel MVP.
2. Keep provider support to one provider until the loop and tools are reliable.
3. Define the exact tool schemas before coding the model loop.
4. Define workspace path-safety rules before enabling `write`, `edit`, or `bash`.
5. Add the first end-to-end smoke test: prompt → tool call → tool result → final
   assistant response.

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
