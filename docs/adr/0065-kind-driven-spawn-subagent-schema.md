# ADR-0065: Kind-driven `spawn_subagent` schema with enforced tool grants

**Date**: 2026-07-19
**Status**: proposed
**Deciders**: operator + agent design review (issue [#665](https://github.com/5omeOtherGuy/iris-agent/issues/665))

## Context

`spawn_subagent` (`src/tools/registry.rs`) grew a wide, flat surface: sixteen
top-level properties covering identity, model selection, a permission grant
(`capability`), a narrowing allowlist (`tools`), budgets
(`max_provider_rounds`, `max_tool_rounds`, `max_tokens`), and a best-of-N group
primitive (`count`, plus the satellite `select_subagent_candidate` tool). Only
`prompt` is required; every other property is model-guessable, which invites
misuse. One live incident: a parent set `max_tokens: 200`/`500` on a spawn,
below the worker's own initial input+output usage, and the worker was
cancelled before doing any work — a budget knob with no safe lower bound the
parent can reason about.

A property-by-property comparison against Claude Code's `Agent` tool, Codex's
`spawn_agent`, and Grok Build's `spawn_subagent`/`TaskTool` found:

- None of the three expose per-worker cumulative token/round budgets to the
  parent model.
- Claude Code resolves a subagent's tool access from static agent-definition
  config (`tools`/`disallowedTools` in frontmatter), hard-filtered out of the
  available tool set before the subagent ever sees a schema
  (`resolveAgentTools`/`filterToolsForAgent` in
  `claude-code/src/tools/AgentTool/agentToolUtils.ts`) — not a prompt-level
  suggestion.
- Claude Code's briefing guidance for the parent model (how to write an
  effective spawn prompt) lives in a dynamically generated tool description
  (`claude-code/src/tools/AgentTool/prompt.ts`), with different rules for
  fresh agents (full context, "brief the agent like a smart colleague who
  just walked into the room") versus context-inheriting forks (a short
  scope-only directive).
- Grok Build restricts tools per agent type in static config, not a
  per-spawn-call parameter.
- Best-of-N is not a native parameter on any competitor's spawn call.

Iris's own runtime already has the pieces this design needs but does not
expose consistently: a session-scoped authenticated model catalog that
already drives the `model`/`provider` enums dynamically
(`SubagentToolsConfig.catalog`, `registry.rs:68-72`); an internal
`CapabilityMode` ceiling (`registry.rs:73`); an unused numeric
`max_nesting_depth` depth counter with no identity restriction on which kinds
may spawn which (`crates/iris-subagent-runtime/src/model.rs:159-164`); and
`WorkerBudgets` fields that default to unbounded
(`model.rs:190-215`).

## Decision

### Kind manifests own defaults; the spawn call overrides them

Introduce a `subagent_type` manifest (config-defined, not hardcoded per
variant) with one entry per kind. Each manifest entry owns:

- a model fallback chain,
- a system prompt,
- a tool profile (the default resolved tool set),
- `allowed_children`: the list of `subagent_type` ids this kind may itself
  spawn (default empty — leaf),
- a default `max_provider_rounds` turn cap (default 200 unless the manifest
  overrides it).

`spawn_subagent` keeps a single, unified tool surface (no separate
"spawn predefined kind" vs "spawn custom" tools, and no per-kind tool as in
an AMP-style design) so that kinds can be added or retuned without touching
tool schema code. AMP-style clarity is reproduced without paying for N
duplicated schemas: each `subagent_type` enum value carries a one-line
"when to use" trigger generated dynamically into the tool description,
never the full system prompt.

### Target schema

```
spawn_subagent
  task           string  required — self-contained work order for a fresh worker
  subagent_type  enum    default "general"; each value carries a one-line
                         "when to use" blurb in the generated tool description
  model          enum    generated from active-credential models only
  provider       enum    present only when a vendor has 2+ active lanes
  effort         enum    default: inherit
  tools          array   abstract tool ids and/or grant shorthands
                         (read_only | read_write | shell | all); replaces
                         the subagent_type's default profile when present,
                         clamped to the parent ceiling
  system_prompt  string  optional override; default: subagent_type's own
                         (general has its own default system prompt, not
                         an inherited copy of the parent's)
  description    string  short label; default: derived from subagent_type
  background     bool    default true
  isolation      enum    default derived from the resolved tool set
  cwd            string  optional
```

Removed from the model-facing schema: `capability` (replaced by `tools`),
`count` and the group-selector parameters on `subagent_status`/
`cancel_subagent`, the `select_subagent_candidate` tool, `max_tokens`,
`max_provider_rounds`, `max_tool_rounds`, and `allow_outside_workspace`
(moves to `subagent_type`/policy level). `kind` is renamed `subagent_type`
to match the parameter name Claude Code already uses for the same concept.

Enum-valued fields (`model`, `provider`, `effort`) carry no per-value
description text; descriptive field names carry the load. Remaining
descriptions are trimmed to one clause.

### Tool grants are enforced, not suggested

`tools` accepts abstract, provider-agnostic tool ids from Iris's own tool
registry (never a raw provider-specific name such as `apply_patch`), which
the runtime resolves to the concrete provider tool after model selection —
the parent never needs to know which concrete edit tool a given provider
uses. It also accepts grant shorthands (`read_only`, `read_write`, `shell`,
`all`) in the same array, expanding to their tool sets, so the common case
does not require enumerating individual tools; shorthand and explicit ids in
the same list union together. An explicit `tools` value replaces the
`subagent_type`'s default tool profile (it may broaden as well as narrow),
clamped to the parent's own ceiling. Enforcement filters the resolved set out
of the worker's tool schema entirely before the worker's first turn — the
same mechanism Claude Code uses, not a system-prompt request the worker can
ignore.

### Turn cap moves off the model-facing surface

The parent no longer sets any budget. `max_provider_rounds` becomes a
runtime-applied default read from the `subagent_type` manifest (200 unless
overridden), applied whenever a spawn does not otherwise carry one from its
manifest entry. This removes the failure class behind the `max_tokens:
200`/`500` incident: a fresh worker starts at round 0, so a turn-based cap
cannot fire before the worker has done any work, unlike a cumulative token
cap sized without knowledge of the worker's own startup cost.

### Best-of-N is deferred, not deleted

Remove `count`, the group-selector parameters, and
`select_subagent_candidate` from the model-facing surface. Keep the existing
runtime group implementation in `iris-subagent-runtime` in place, dormant.
`plan_subagent_apply`/`apply_subagent` are unaffected — they already operate
per worker.

### Model/provider enum generation

`model` is generated from active credentials only; a model with no live
lane never appears in the enum (fixes the class of failure where a parent
picks a model whose only lane has no key/token). Lane selection prefers
OAuth over API for the same vendor; the API lane becomes selectable only
after an explicit, session-sticky approval. `provider` is included in the
schema only when at least one vendor exposes two simultaneously active
lanes; otherwise it is omitted. This is an extension of the existing
session-captured-catalog mechanism (`registry.rs:68-72`), not new machinery.

### Explicit exclusions

Do not build guardrailed multi-step workflows (deferred to a future ADR).
Do not add a per-tool GUI/CLI editor for kind manifests in this change. Do
not rename the internal `WorkerKind` Rust type in this change — only the
JSON schema key becomes `subagent_type`; the internal rename is a separate,
optional follow-up.

## Alternatives Considered

### Keep `capability` as a coarse grant alongside a narrowing `tools` allowlist
- **Pros**: Matches the current implementation; no schema change.
- **Cons**: Two overlapping fields for one decision (what can this worker
  touch), and `tools` can only narrow, never model a "read-only except for
  one extra tool" case without also touching `capability`.
- **Why not**: `tools` alone, with shorthand grant tokens, covers every case
  `capability` covered plus the cases it didn't.

### Expose one tool per `subagent_type` (AMP-style)
- **Pros**: Maximally self-evident tool selection; no enum to parse.
- **Cons**: N duplicated schemas for every shared field (task, model, effort,
  tools, isolation, cwd); adding or retuning a kind requires shipping a new
  tool definition, defeating the goal of adjusting kind profiles dynamically
  at runtime.
- **Why not**: A single tool with per-`subagent_type` one-line triggers in a
  dynamically generated description reproduces the clarity without the
  schema duplication, and remains reversible if kind count grows enough to
  change that tradeoff.

### Let the parent set `max_provider_rounds`/`max_tokens` per spawn, with a documented safe floor
- **Pros**: Preserves per-task tuning by the parent.
- **Cons**: The incident that motivated this ADR was exactly a parent
  misjudging a safe floor; no competitor exposes an equivalent knob, and a
  cumulative token cap has no reference frame the parent can compute in
  advance (it does not know the worker's own startup token cost).
- **Why not**: A kind-manifest-owned default turn cap gives the same runaway
  protection without a parameter the parent can set below the worker's own
  floor.

### Keep best-of-N as a first-class spawn parameter
- **Pros**: Atomic identical-N-spawns guarantee; enforced inspect-before-select
  gate via `select_subagent_candidate`.
- **Cons**: `count`, its group-selector parameters on two other tools, and a
  dedicated selection tool are schema surface for a feature usage did not
  justify keeping model-facing, and no competitor's spawn call exposes an
  equivalent native parameter.
- **Why not**: Remove from the model-facing surface now; keep the runtime
  group implementation dormant so the feature can return as a skill-driven
  workflow later without re-implementing scheduling.

## Consequences

### Positive
- One property (`tools`) replaces two overlapping ones (`capability`,
  `tools`), with hard enforcement instead of a documentation-only allowlist.
- The runaway-token incident's failure class cannot recur: no parent-settable
  budget knob remains on the schema.
- `subagent_type` manifests make adding or retuning a kind a config change,
  not a tool-schema change, satisfying the dynamic-reconfiguration goal.
- Schema token cost drops: six properties removed outright, enum descriptions
  dropped, remaining descriptions shortened.
- Provider-specific tool naming (for example `apply_patch` vs `edit`) is
  fully hidden from the parent, removing a class of spawn calls invalidated
  by model/provider fallback resolution.

### Negative
- `tools` shorthand-plus-explicit-id union semantics is a new small piece of
  parsing/validation logic that did not exist before.
- Manifests are a new configuration surface with their own validation needs
  (unknown `subagent_type` in `allowed_children`, cyclical
  `allowed_children`, missing default model in a fallback chain).
- Removing `select_subagent_candidate` and group parameters is a breaking
  change for any existing caller relying on best-of-N; no migration path is
  provided beyond "spawn N times manually."

### Risks
- If `tools` override semantics (broaden vs. narrow-only) are implemented
  inconsistently between validation and runtime enforcement, a worker could
  receive a grant wider than the manifest default without a visible ceiling
  check; the parent-ceiling clamp must be enforced at the same point as tool
  resolution, not only at validation time.
- A 200-round default turn cap is unverified against real usage; it is a
  safety backstop, not a tuned value, and should be checked against
  telemetry once available.
- Provider-agnostic tool ids must stay in lockstep with each provider's
  concrete tool surface (for example, if a provider adds a new mutating tool
  outside the existing `edit`/`apply_patch` mapping) or the abstraction leaks.
