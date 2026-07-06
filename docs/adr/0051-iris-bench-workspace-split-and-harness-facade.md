# ADR-0051: Split the crate into a workspace; drive the benchmark through a `harness` facade

**Date**: 2026-07-06
**Status**: accepted
**Deciders**: iris-agent maintainers

## Context

The tokens-per-task benchmark (issue #210) lived entirely under `#[cfg(test)]`
inside the `iris-agent` binary crate: fixtures, workload catalog, arm toggle,
runner, and analysis were test-only code driven by environment variables
(`IRIS_BENCH_*`). That shape has three problems as the benchmark grows into a
tool people actually run:

- It is not runnable outside `cargo test --ignored`, so model/thinking/arm/N/
  concurrency selection is env-var stringly-typed, and there is no run TUI or
  report surface.
- Its instrumentation (observer, approval gate, arm switch) reaches into
  `nexus`, `tools`, `config`, and `mimir` internals. Any external driver would
  have to depend on the whole binary's private surface.
- The user-facing `iris` binary must stay free of benchmark dependencies (TUI
  grid, charts, HTML report, fixtures).

We want a standalone `iris-bench` tool that selects model/reasoning/workloads/
arms/parallelism/N, shows a live grid, and emits a static HTML report — without
leaking runtime internals or dragging bench dependencies into `iris`.

## Decision

Convert `iris-agent` from a bin-only crate into a two-member Cargo workspace and
expose one curated public seam:

- `iris-agent` becomes `lib` + `bin`. `src/main.rs` is a thin shim over
  `iris_agent::run_cli()`; the CLI body moved to `src/lib.rs`. Internal modules
  stay private.
- A new `src/harness.rs` is the **only** public surface `iris-bench` depends on.
  It wraps the internal agent/provider/tool machinery and exposes just: `Arm`,
  `CellSpec`, `CellResult`, an opaque `ModelSelection`, `selection_for_spec`,
  `validate_model`, and `run_cell`. `nexus`/`tools`/`config`/`mimir` types never
  become public.
- The one production-code change is un-gating `ToolState::with_reduce_output`
  (the arm switch), which stays `pub(crate)`. `BenchObserver` and
  `ZeroPromptGate` move into `harness` as non-test code; the existing
  `#[cfg(test)]` replay bench keeps working via a re-export in
  `bench_tokens/observer.rs`.
- `iris-bench` is a new standalone binary crate that owns its own script-free
  workload catalog, fixture materialization, parallel engine (bounded OS-thread
  pool + single-writer JSONL log), run TUI, analysis, and HTML report. It
  depends on `iris-agent` only through `harness`.

## Alternatives Considered

### Alternative 1: Keep the benchmark under `#[cfg(test)]`, driven by env vars
- **Pros**: zero structural change; nothing new to maintain.
- **Cons**: not runnable as a tool; stringly-typed config; no run/report UI;
  instrumentation stays welded to private internals.
- **Why not**: the goal is a real tool with typed selection and a report, which
  test-only code cannot provide.

### Alternative 2: Add a `bench` subcommand to the `iris` binary (Option C)
- **Pros**: one crate, no workspace.
- **Cons**: reverses the dependency edge — the user-facing binary would depend
  on bench-only code (TUI grid, charts, HTML, fixtures) and their dependencies,
  bloating the shipped `iris` and coupling release cadence to bench churn.
- **Why not**: the shipping binary must stay bench-free; a cargo feature can be
  revisited later if in-binary access is ever needed.

### Alternative 3: Make `nexus`/`tools`/`config`/`mimir` items broadly public
- **Pros**: no facade to write; `iris-bench` calls internals directly.
- **Cons**: freezes large internal surfaces as public API; violates the tier
  boundaries in `docs/ARCHITECTURE.md`; every refactor risks the external crate.
- **Why not**: a single curated `harness` seam preserves the boundary and keeps
  the blast radius to one file.

## Consequences

### Positive
- `iris-bench` is a normal binary: typed model/reasoning/workload/arm/N/
  concurrency selection, a live grid, JSONL log, and a self-contained HTML
  report — with no benchmark code or dependencies in the shipped `iris`.
- The runtime's internals stay private; the only cross-crate contract is one
  small, documented `harness` module.
- The existing `#[cfg(test)]` replay bench is untouched and still gates.

### Negative
- One more crate and a workspace to maintain; the `harness` facade must be kept
  in sync when internal signatures change.
- `ToolState::with_reduce_output` is now compiled into non-test builds (still
  `pub(crate)`, so not externally reachable).

### Risks
- `run_cell` builds a fresh current-thread Tokio runtime and must be called from
  an ordinary OS thread, never a Tokio worker (nested-runtime panic). The engine
  pool uses std threads to honor this; the constraint is documented on
  `block_on`.
- `harness` intentionally omits the test-only `Workload`/`ScriptedProvider`
  replay types; `iris-bench` owns a script-free `WorkloadSpec` instead, so the
  two catalogs can drift and must be reconciled deliberately.
