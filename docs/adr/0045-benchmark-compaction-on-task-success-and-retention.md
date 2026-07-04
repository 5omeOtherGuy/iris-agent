# ADR-0045: Benchmark compaction on task success and load-bearing-detail retention

**Date**: 2026-07-04
**Status**: proposed
**Deciders**: Iris maintainers, Claude agent session

## Context

The Milestone 2 gate is "reduce prompt tokens without reducing task success"
(`docs/ROADMAP.md`). ADR-0036 rule 5 makes reduction measurable, and the token-efficiency
benchmark recipe (`src/tools/bench_support.rs`, `docs/benchmarks/*`, the
`token-efficiency-benchmark` skill) proves per-tool output reduction with minimum-reduction
bars and `assert_survives_verbatim` needles that guard actionable content.

Compaction (ADR-0009, ADR-0041) has no equivalent gate. Nothing scores whether a compacted
trajectory still completes its task, or whether load-bearing facts survive the summary. Token
reduction alone is the wrong target: a summary that halves tokens but drops a file path or a
prior decision can reduce task success, which is the failure ADR-0041 flags.

A reverse-engineering pass over the Cursor agent CLI shows it optimizes compaction against
task success, not token count, reporting a large reduction in compaction errors. Iris uses
provider models and cannot train the summarizer, but the evaluation shape transfers: measure
the compacted trajectory, not just its size.

Tracked in #372.

## Decision

Extend the token-efficiency benchmark discipline to compaction, through the production
compaction seam.

- **Task-success scenario.** Add a long-horizon, compaction-heavy scenario that forces at
  least one auto-compaction (`src/wayland/mod.rs`) and scores post-compaction task success,
  not token delta alone.
- **Retention needles.** Reuse `assert_survives_verbatim`: after compaction, required facts
  (file paths, prior decisions, identifiers) must survive in rebuilt context, whether in the
  ADR-0044 carry or the summary. Retention is a pass/fail contract, like the tool-output
  needles.
- **Retention vs. recoverability.** Once recall (ADR-0046) and folds (ADR-0048) exist, a
  needle passes in one of two ways: it survives in rebuilt context, or it sits behind a
  named reference (recall handle, fold handle) that itself survives verbatim. The two
  outcomes are reported separately; a needle that is merely recoverable is not counted as
  retained.
- **A/B the summarizer arms.** Report token delta and retention for `provider` and `excerpts`
  (ADR-0041), `provider + structured carry` (ADR-0044), and `provider + carry +
  microcompaction` (ADR-0048), so the cost/quality trade of each arm is measured, not
  assumed.
- **Report dimensions, not just arms.** Each arm reports by compaction generation
  (ADR-0047) and covered-range size (summary quality degrades with the range one summary
  must carry), plus two cache-economics measurements from `ProviderUsage` cache read/write
  splits: the summarization request's cache-hit rate, and post-compaction cache-write
  amplification. Both are measured because the warm-cache premise is narrow: the
  summarization request rides the live cached prefix only when the covered range starts at
  the prefix (`provider_summary`, `src/wayland/mod.rs`), which holds for a session's first
  compaction and rarely after; and the `keep_target` hysteresis rewrites the retained tail
  to cache on every compaction.
- **Committed report, tests are the contract.** One report under
  `docs/benchmarks/<issue-slug>.md` with the table, the regeneration command, and which
  numbers are asserted; the tests are the contract, the doc is the snapshot (skill:
  token-efficiency-benchmark). Tokens are ratios via `bench_support::est_tokens`; no absolute
  counts as fact.

This ADR defines the gate shape. Measured results land with the benchmark implementation;
until then the claim stays a goal (skill: write-documentation, Claims).

## Alternatives Considered

### Alternative 1: Keep token-delta-only benchmarks
- **Pros**: Already built; simplest.
- **Cons**: A token win that drops a load-bearing fact scores as a win while reducing task
  success; measures the wrong thing for compaction.
- **Why not**: The gate is task success, not size.

### Alternative 2: Score task success only, drop the retention needles
- **Pros**: Fewer assertions; task success is the real target.
- **Cons**: Task-success scenarios are noisy and expensive; a needle failure localizes the
  regression to a dropped fact, which a pass/fail task run does not.
- **Why not**: Needles are the cheap, deterministic contract that explains a task-success
  regression.

### Alternative 3: A separate compaction eval harness outside bench_support
- **Pros**: Freedom to model trajectories.
- **Cons**: A second benchmark system; duplicates the estimator, report format, and
  reading-guide discipline already standardized by ADR-0036 rule 5.
- **Why not**: Extend the shared recipe; a trajectory scenario is a new corpus, not a new
  harness.

## Consequences

### Positive
- The compaction/token path gets the same measured gate as tool outputs, closing the
  Milestone 2 acceptance on evidence.
- A/B arms make the ADR-0041/0043 trade explicit and regression-testable.
- Retention needles catch a dropped load-bearing fact deterministically.

### Negative
- Long-horizon scenarios are slower and noisier than single-tool corpora; the scenario set
  must stay small and representative.
- Provider-arm runs cost model calls; the A/B may need a recorded or fake-provider lane to
  stay deterministic in CI.

### Risks
- A scenario that is too easy hides compaction regressions; mitigate by requiring at least one
  forced compaction and needles that target facts present only before the covered range.
- Task-success scoring can be flaky; mitigate by asserting retention needles as the
  deterministic floor and treating task success as the trend measure.
