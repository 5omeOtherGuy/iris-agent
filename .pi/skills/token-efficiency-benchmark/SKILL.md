---
name: token-efficiency-benchmark
description: Build or extend a token-efficiency benchmark for an Iris tool result (ADR-0036 rule 5, "reduction is measured"). Use when adding or changing any output-reduction path — bash filters, windowing, summaries, handle offload — when a savings claim needs proof, or when extending the ADR-0037 corpus. Covers real-fixture capture, the shared bench_support helpers, minimum bars, verbatim-survival needles, overhead bounds, and the committed report.
---

# Token-Efficiency Benchmark

ADR-0036 makes token efficiency a design requirement for every tool result and
rule 5 makes it measurable: savings claims cite a benchmark against
representative outputs, with zero loss of actionable content. This skill is the
recipe for building that benchmark.

Reference implementation: `src/tools/bash/filter/corpus.rs` (ADR-0037 bash
filter corpus) over the shared core `src/tools/bench_support.rs`, reported in
`docs/benchmarks/adr-0037-bash-filter-tokens.md`.

## When to use

- A tool gains or changes an output-reduction path (filtering, windowing,
  summarizing, handle offload) and ADR-0036 rule 5 requires proof.
- Extending the bash filter corpus (new filters, PR 2 structured filters).
- Any doc or PR wants to claim a savings percentage.

## The recipe

1. **Capture real fixtures.** Run the real command/tool and commit the raw
   captured output under `<module>/corpus/*.txt`. Never hand-write fixtures;
   representative means captured. Include at least:
   - one noisy success (the class the reduction exists for),
   - one failure (failing test, compile error, non-zero exit),
   - one class the reduction deliberately does not cover (passthrough proof).
   Redirect with `> fixture 2>&1` so the capture matches what the tool sees.
   Exclude the corpus directory from the spell-checker (`typos.toml`
   `[files] extend-exclude`) — captured output is not prose.

2. **Pair each fixture with its invocation context.** Whatever drives the
   reduction path in production (command string + exit status for bash
   filters; offsets/limits for windowing). Keep this in a per-corpus `Sample`
   struct next to the tests — it is tool-specific by design and does not
   belong in the shared core.

3. **Measure through the production seam.** Call the same entry point the
   runtime calls (`filter_output`, not the engine internals), so every
   fail-safe and guard is inside the measurement.

4. **Assert, don't just print** (`crate::tools::bench_support`, test-only):
   - `assert_min_reduction(class, before, after, min_pct)` — minimum bars,
     never exact figures. Bar for noisy classes (build/test/install/VCS
     logs): 60. Failure classes get no bar — failure detail is exempt from
     reduction by contract.
   - `assert_survives_verbatim(class, out, needles)` — needles are the
     quality-loss contract: error messages, `file:line`, failing test names,
     summaries. Every failure fixture needs them.
   - Passthrough classes: assert the reduction path returns the input
     untouched (or `None`), so uncovered classes stay honest in the report.
   - `assert_call_overhead_under(class, bar, || ...)` — warm lazy state
     first (compiled registries, caches); the helper takes best-of-three.
     Reference bar: 10 ms per call, debug build.

5. **Commit the report.** One markdown file per benchmark under
   `docs/benchmarks/<adr-or-issue-slug>.md`: the table (from
   `report_header()`/`report_row()`, printed by a `*_benchmark_report` test
   run with `-- --nocapture`), the regeneration command, and a short
   reading-guide noting which numbers are asserted and which classes pass
   through and why. The doc is a snapshot; the tests are the contract.

## Rules

- Tokens are estimated at 4 bytes/token (`bench_support::est_tokens`). Only
  ratios are meaningful; never quote absolute token counts as fact.
- State every measurement condition the numbers depend on (debug build,
  warmed registry, best-of-three) in the report.
- A benchmark without survival needles is not acceptable: reduction numbers
  alone cannot show "without quality loss".
- Do not benchmark synthetic worst cases to inflate savings; the corpus is
  representative or it is nothing.
- Per-result compression only. End-to-end task-success measurement (does the
  model still complete the task from reduced context) is the Milestone 2
  benchmark plan (#210), not this harness.

## Extending vs. generalizing

`bench_support` stays tool-agnostic and small: estimation, bars, needles,
overhead, report rows. Fixture capture, `Sample` shapes, and dispatch context
stay with each corpus. If a new consumer needs more shared machinery, move the
specific helper into `bench_support` when the second consumer exists — not
before.
