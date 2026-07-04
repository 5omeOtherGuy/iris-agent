# Issue #340 find compaction — token benchmark

Measured over captured real `find`-style listings in `src/tools/find_corpus/`
(relative paths from a checked-out `codex-rs` tree). Tokens estimated at
4 bytes/token; only the ratios matter. The numbers below are asserted (as a
minimum bar and a flat-stays-flat check) by the tests in `src/tools/find.rs`,
built on the shared measurement core in `src/tools/bench_support.rs` (recipe:
the `token-efficiency-benchmark` skill in `.pi/skills/`). Regenerate the table
with:

```
cargo test --bin iris find_benchmark_report -- --nocapture
```

`before` is the flat listing (one path per line, today's output); `after` is
the grouped-by-directory listing of the same set (`dir/ a.rs b.rs`). Both render
the whole fixture uncapped to isolate the representation change from the
byte/line rail.

| class | tokens before | tokens after | reduction | via |
|---|---|---|---|---|
| find (concentrated .rs) | 5094 | 2361 | 54% | group-by-dir |
| find (one file per dir) | 7815 | 7815 | 0% | (flat) |

## Reading the table

- **Grouping shipped.** On a real concentrated tree (456 `.rs` files sharing
  parent directories) grouping removes the repeated directory prefix and cuts
  the listing ~54%. `bench_concentrated_grouping_reduces` asserts a ≥ 40%
  minimum bar and that sampled real file names survive grouping verbatim (zero
  quality loss).
- **Flat stays flat.** With one file per directory, `dir/ name` costs one byte
  more than `dir/name`, so grouping never wins and the flat form is kept
  (`grouping_not_used_when_flat_is_smaller`,
  `bench_singletons_flat_stays_flat`). The runtime picks the smaller of the two
  forms per result set, so a set that would not shrink passes through flat.
- **Bars are minimums, never exact figures.** The 40% bar is the contract; the
  54% is a snapshot of the current fixture.

## Truncation summary (the primary win)

Grouping is secondary. The headline change is that a truncated result now ends
with an exact, actionable summary instead of a bare `[output truncated]` that
forced a blind re-run. Example over the concentrated fixture at a 400-byte cap
(the shipped caps are 2000 lines / 50 KB):

```
codex-rs/core/src/agent/ agent_resolver.rs control.rs control_tests.rs mod.rs registry.rs ...
codex-rs/core/src/agent/control/ execution.rs execution_tests.rs legacy.rs residency.rs ...
codex-rs/core/src/ agents_md.rs agents_md_tests.rs apply_patch.rs apply_patch_tests.rs attestation.rs
codex-rs/core/src/apps/ mod.rs render.rs

[456 matches, 22 shown, 434 omitted]
omitted by dir: codex-rs/core/src/ (98), codex-rs/core/tests/suite/ (94), codex-rs/core/src/tools/handlers/ (52), codex-rs/core/src/context/ (30), codex-rs/core/src/tools/ (22), ...
```

The summary carries the exact total match count and the top directories by
omitted-match count, so the model can narrow the glob
(`codex-rs/core/src/**`) instead of re-running blindly. No matches are dropped
without a count. `truncation_summary_reports_exact_total_and_top_dir` asserts
the total is exact and the named top directory is the plurality of omitted
matches (counts are correct, not decorative).

## What is asserted vs. reported

- **Asserted** (tests, the contract): concentrated grouping ≥ 40% reduction with
  verbatim survival; singletons keep the flat form; truncation summary carries
  the exact total and correct top-directory omitted counts; under-cap results
  stay byte-identical to the historical flat listing.
- **Reported** (this doc, a snapshot): the 54% figure and the example summary.

## Measurement conditions

Debug build. Tokens are `bench_support::est_tokens` (4 bytes/token); only ratios
are meaningful. Fixtures are captured real listings, not synthetic worst cases.
