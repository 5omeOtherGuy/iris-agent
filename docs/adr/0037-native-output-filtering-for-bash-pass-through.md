# ADR-0037: Native output filtering for bash pass-through commands

**Date**: 2026-07-04
**Status**: proposed
**Deciders**: iris-agent maintainers

## Context

ADR-0036 sets the goal: maximum token efficiency without quality loss. Native
tools already bound their own output. The remaining gap is `bash`: it relays raw
output from arbitrary programs (`cargo test`, `git diff`, `npm test`,
`terraform plan`), where noise routinely reaches thousands of tokens per call.
Today the only reduction is the tail cap in the bash tool
(`truncate_tail`, 2000 lines / 50 KB) — a safety rail, not compression; a noisy
log under the cap enters the transcript untouched.

Prior art: RTK (rtk-ai/rtk, Apache-2.0) is a Rust CLI proxy that filters 100+
dev commands and reports 60–90% token reduction. Its filter designs are sound;
its delivery model (separate binary, hook-based command rewriting, trust store
for project filters) exists only because it does not own the runtime. Iris does,
so the equivalent capability fits at one seam: inside the bash tool, after
capture, before truncation and transcript encoding.

## Decision

Implement command-output filtering natively in the Nexus bash tool.

- **Seam**: one filter stage in `src/tools/bash/`, applied to captured output
  after the command exits and before `truncate_tail`. The cap remains as the
  fail-safe backstop. Applies to one-shot runs, persistent sessions, and
  finalized background jobs.
- **Dispatch**: key filters on the parsed command (program + subcommand) from
  the command string. Filter output post-hoc; never rewrite the command the
  model asked to run.
- **Filter engine**: a declarative, data-driven line-filter pipeline (ordered
  stages: strip ANSI, regex replace, short-circuit match with error guards,
  keep/strip lines, per-line truncation, head/tail, empty-result message),
  with filter definitions as embedded data files. Port the design and vendor
  the Apache-2.0 filter definitions from RTK with attribution rather than
  authoring 70+ filters from scratch.
- **Structured filters in Rust** for the highest-value commands where regex
  stripping is insufficient (cargo test/build, git status/diff/log, npm/pnpm
  test): parse and summarize, keeping full failure detail.
- **Quality-loss guards** (the "without quality loss" half of the goal):
  - Fail-safe: any filter error returns raw output.
  - Exit codes and stderr failure signals are never altered.
  - Error/failure lines are exempt from stripping (guard patterns on every
    short-circuit rule).
  - Escape hatch: a `raw` parameter on the bash tool bypasses filtering for
    one call; full raw output also remains reachable via ADR-0011 handles.
- **Acceptance bar**: on a benchmark corpus of representative command outputs,
  match or beat RTK's reduction per command class, with zero regressions on a
  task-success suite (failing tests, compile errors, and diffs must remain
  actionable from filtered output alone). Claims of savings cite this
  benchmark, per repo doc rules.
- **Skip** RTK's non-filter machinery: hooks, command rewriting, trust and
  integrity stores, telemetry, tracking database, analytics.

## Alternatives Considered

### Alternative 1: Ship or require the RTK binary
- **Pros**: No porting; upstream maintenance.
- **Cons**: Separate install; per-command process overhead; hook/rewrite model
  is redundant inside a runtime; unverifiable external dependency in the tool
  execution path.
- **Why not**: Iris owns the seam; a proxy adds a dependency without adding
  capability.

### Alternative 2: Link RTK as a library
- **Pros**: Reuse code directly.
- **Cons**: RTK is a binary crate with no library target; pulls rusqlite,
  ureq, telemetry, and hook machinery Iris must not ship.
- **Why not**: Not linkable as published; carving out a lib means a fork with
  most of the code deleted — porting the engine is smaller.

### Alternative 3: Dedicated native tools per command (git tool, test tool)
- **Pros**: Best-structured results; strongest contracts.
- **Cons**: Covers a handful of commands; the model still uses bash for the
  long tail; every new tool grows the tool surface and prompt.
- **Why not**: Complementary, not sufficient. Structured Rust filters inside
  bash capture most of the value without new tool surface; dedicated tools
  remain an option later where structure pays off.

### Alternative 4: Prompt the model to run quieter commands (`--quiet`, `2>/dev/null`)
- **Pros**: Zero code.
- **Cons**: Unreliable adoption; discards error detail exactly when it is
  needed; decays over long sessions.
- **Why not**: Unenforceable and quality-lossy — fails both halves of the goal.

## Consequences

### Positive
- Token reduction on the highest-volume context source (build/test/git output)
  with no user setup, no external binary, no command rewriting.
- One tested enforcement point; filters are data files, cheap to extend.
- Same-or-better outcome than the proxy approach: native dispatch sees the
  exact command and cwd, filters session and job output too, and cannot be
  bypassed by the model forgetting a prefix.

### Negative
- Iris takes on filter maintenance as command output formats drift upstream.
- Vendored filter data needs periodic re-sync with upstream fixes.

### Risks
- Over-filtering hides a signal the model needed. Mitigation: guard patterns,
  fail-safe raw fallback, `raw` escape hatch, and the task-success suite in the
  acceptance bar.
- Filter regex cost on hot paths. Mitigation: compile-once embedded filters;
  benchmark overhead alongside savings (proxy prior art sustains <10 ms).
