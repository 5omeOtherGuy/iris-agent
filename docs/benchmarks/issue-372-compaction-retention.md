# Issue #372 compaction retention — needle benchmark (slice A)

Measured over a deterministic long-horizon scenario that runs through the
production auto-compaction seam (`crate::wayland::Harness::maybe_auto_compact`,
`src/wayland/mod.rs`) on the fake-provider lane — no live model calls. Tokens
estimated at 4 bytes/token; only the ratios matter. The numbers below are
asserted (as minimum bars and retention needles) by the tests in
`src/compaction_bench.rs`, built on the shared measurement core in
`src/tools/bench_support.rs` (recipe: the `token-efficiency-benchmark` skill in
`.pi/skills/`; design: ADR-0045). Regenerate the table with:

```
cargo test --bin iris compaction_retention_benchmark_report -- --nocapture
```

This is slice A: it fixes the retention-needle contract and the two base
summarizer arms — `provider` and `excerpts` (ADR-0041). The A/B carry and
microcompaction arms and the report dimensions are deferred (see below).

## What the scenario does

A three-turn session forces one auto-compaction. The first prompt is large and
carries four load-bearing facts that exist only in it — a task id, a file path,
a symbol, and a prior decision (`TASK-8291`, `crates/orbit/src/telemetry/sink.rs`,
`reconcile_ledger`, `ULID-keys ADR-0044`). It dwarfs the keep target, so
compaction covers exactly that first prompt and the summary replaces it in the
rebuilt context. The needles are asserted to survive verbatim in the rebuilt
context, and to be absent from the retained tail — so retention is proven
through the summary, not leftover context.

The fake provider does not hard-code the handoff: on a summarization request it
receives the covered range the production seam passed, asserts each needle is
present in that covered input, and only then echoes it. A seam that passes the
wrong covered range or drops the opener fails the fake's assertion rather than
producing a self-fulfilling pass.

| class | tokens before | tokens after | reduction | via |
|---|---|---|---|---|
| compaction/excerpts | 405 | 55 | 86% | excerpts |
| compaction/provider | 405 | 55 | 86% | provider |

provider/excerpts summary token ratio: 1.00 (55 vs 55 est tokens).
Auto-compactions fired: excerpts = 1, provider = 1.

## Reading guide

- **Retention needles** (`needles_survive_verbatim_in_rebuilt_context_*`): the
  four facts survive verbatim in the rebuilt context on both arms, and none
  leak into the retained tail. This is the pass/fail contract; a summarizer
  that drops a load-bearing fact fails here, deterministically.
- **Asserted reduction bars** (`each_arm_clears_the_minimum_reduction_bar`):
  each arm shrinks the covered range by ≥ 60% (both clear it at 86% today).
  Bars are minimums, never exact figures; a summarizer regression that stops
  compressing the covered range trips it.
- **Cross-arm ratio bound** (`provider_arm_stays_within_a_bounded_ratio_of_excerpts`):
  the provider handoff stays within 1.5x of the deterministic excerpts floor
  (1.00 today). A ceiling on provider/excerpts, i.e. a floor on the win — a
  regression guard, not a tight fit.
- **Forced compaction** (`scenario_forces_at_least_one_auto_compaction_on_both_arms`):
  at least one auto-compaction fires through the production seam on each arm.
- **Genuine provider arm** (`provider_arm_uses_the_provider_summarizer_not_the_excerpts_fallback`,
  plus an in-`run_scenario` guard on every arm): the provider arm must invoke
  the provider summarizer (`summary_calls >= 1`) and emit the provider marker
  (`[compacted summary ...]`); the excerpts arm must never call the provider
  (`summary_calls == 0`) and must emit the excerpts marker
  (`[auto-compacted summary ...]`). If provider summarization silently fell
  back to excerpts, the provider/excerpts ratio would be an
  excerpts-vs-excerpts comparison — this guard fails it instead.
- Both arms report an identical 86% here because the covered range is a single
  large message: the `excerpts` arm truncates it to a bounded excerpt and the
  `provider` arm returns a short structured handoff of comparable size. The
  arms diverge on retention *quality* (structured handoff vs. truncated raw),
  which the needles guard, not on token size in this covered-range shape.

## Measurement conditions

Debug build, fake-provider lane, fixed prompt sizes and a fixed 300-token
budget, so the covered range, both summaries, and every ratio are reproducible
in CI. Tokens are `bench_support::est_tokens` estimates (4 bytes/token); no
absolute count is quoted as fact.

## Deferred to slice B (ADR-0045, #372)

Named here so the gate shape is on record; not built in this slice:

- **Arms**: `provider + structured carry` (ADR-0044) and `provider + carry +
  microcompaction` (ADR-0048, after #378).
- **Dimensions**: compaction generation (ADR-0047); covered-range size (summary
  quality degrades with the range one summary must carry); and the two
  `ProviderUsage` cache-economics measurements — the summarization request's
  cache-hit rate and post-compaction cache-write amplification (the warm-cache
  premise holds only for a session's first compaction; `keep_target` hysteresis
  rewrites the retained tail to cache on every compaction).
- **Retention vs. recoverability**: once recall (ADR-0046) and folds (ADR-0048)
  exist, a needle passes either retained in rebuilt context or behind a named
  reference that survives verbatim; the two outcomes reported separately.
