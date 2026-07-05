# Issue #372 compaction retention — needle benchmark (slice B)

Slice B extends the slice-A scaffold
(`docs/benchmarks/issue-372-compaction-retention.md`) to the full gate shape
ADR-0045 defines: the two remaining summarizer arms, the report dimensions
(generation, covered-range size, cache economics), and the retained vs.
recoverable-behind-reference split. Everything runs on the deterministic
fake-provider lane through the production auto-compaction seam
(`crate::wayland::Harness`) — no live model calls — and reads its numbers from
the seam's own `CompactionApplied` events and durable transcript, never
fabricated. Tokens are `bench_support::est_tokens` estimates (4 bytes/token);
only the ratios matter. The numbers below are asserted (minimum bars, retention
needles, and dimension checks) by the tests in `src/compaction_bench.rs`, built
on the shared measurement core in `src/tools/bench_support.rs`.

Regenerate the tables with:

```
cargo test --bin iris compaction_slice_b_benchmark_report -- --nocapture
```

Run the asserting tests (the contract; the tables are the snapshot) with:

```
cargo test --bin iris compaction_bench
```

## The four arms

Measured on each arm's first compaction (the warm-cache case). `provider` and
`excerpts` cover a single large needle-bearing message (the slice-A text
scenario). `provider+carry` and `provider+carry+microcompaction` cover a
tool-bearing range seeded through resume: two successful reads sit in the
covered prefix, one carried verbatim (never re-read) and one superseded by a
later read of the same path (so microcompaction folds it). Before/after tokens
for the seeded arms come straight from the seam's `CompactionApplied` estimates,
because the microcompaction arm folds the covered range before compaction and so
has no single verbatim string to re-measure.

| class | tokens before | tokens after | reduction | via |
|---|---|---|---|---|
| compaction/excerpts | 405 | 94 | 77% | excerpts |
| compaction/provider | 405 | 92 | 77% | provider |
| compaction/provider+carry | 494 | 112 | 77% | provider+carry |
| compaction/provider+carry+microcompaction | 349 | 102 | 71% | provider+carry+microcompaction |

Carried paths: `provider+carry` = 2, microcompaction = 1 (the folded read is no
longer carry-eligible, so only the un-superseded path is carried). Folds:
microcompaction = 1.

Every arm clears the 60% covered-range reduction bar
(`four_arms_each_clear_the_minimum_reduction_bar`). The reduction is 77%, not
the 86% slice A first reported, because the recall reference (ADR-0046, #373)
now rides inside every rebuilt summary: the marker plus the summary framing is a
fixed floor of ~81 estimated tokens. That floor is load-bearing — it is the only
anchor telling the model the covered originals are recoverable — so it is
counted, not stripped. It also explains why a very small covered range barely
reduces (see the generation table).

## Retained vs. recoverable-behind-reference

A needle passes retention in one of two separately reported ways (ADR-0045):

- **Retained** — survives verbatim in rebuilt context. The carried path
  `crates/orbit/src/telemetry/sink.rs` is retained verbatim in the ADR-0044
  carry block on both seeded arms
  (`carry_path_is_retained_verbatim_in_rebuilt_context`), as are the four
  slice-A load-bearing facts through the summary.
- **Recoverable-behind-reference** — folded or compacted out of rebuilt context,
  but reachable through a named reference that itself survives verbatim. The
  microcompaction arm folds the superseded read: its detail (`FOLD-ONLY-DETAIL-…`)
  is absent from rebuilt context, while the recall marker (`recall(handle=…)`)
  is retained verbatim as the recovery path
  (`folded_detail_is_recoverable_behind_a_reference_not_retained`). In the
  fold-only view (high budget, no compaction) the fold stub survives verbatim and
  names both the workspace-relative path and the recall tool as the two recovery
  references (`microcompaction_fold_stub_names_the_recoverable_path`).

A merely recoverable needle is never counted as retained.

## Covered-range SIZE dimension

Slice A noted that a single-message covered range makes `provider` == `excerpts`
(both return one bounded form of comparable size). As the covered range grows,
the excerpts summary grows (~160 chars/message) while the provider handoff stays
fixed, so the excerpts/provider summary-token ratio climbs. The arms genuinely
separate on size (`arms_separate_as_the_covered_range_grows`).

| covered range | provider est tokens | excerpts est tokens | excerpts/provider ratio |
|---|---|---|---|
| single large message | 92 | 94 | 1.02 |
| 6 text turns | 81 | 186 | 2.30 |
| 10 text turns | 81 | 370 | 4.57 |

## Compaction GENERATION dimension (ADR-0047)

Driving repeated over-budget turns forces successive compactions; the generation
ordinal is read from `CompactionApplied.generation`
(`generation_ordinal_advances_across_compactions`).

| generation | covered msgs | before est tok | after est tok | reduction | carried |
|---|---|---|---|---|---|
| 1 | 4 | 138 | 81 | 41% | 0 |
| 2 | 3 | 137 | 81 | 41% | 0 |
| 3 | 2 | 87 | 81 | 7% | 0 |
| 4 | 2 | 87 | 81 | 7% | 0 |

Reduction degrades as later generations cover smaller ranges: the summary's
~81-token floor (recall marker + framing) approaches the covered-range size, so
generations 3–4 barely clear break-even. This is the measured cost of the
recoverability guarantee on small ranges, reported honestly rather than hidden.

## Cache economics — measured vs. pending

The warm-cache premise is narrow (ADR-0041/ADR-0045): the summarization request
rides the live cached prefix only when the covered range starts at that prefix,
and the `keep_target` hysteresis rewrites the retained tail to cache on every
compaction. What the deterministic lane genuinely exposes is **measured**:

- **Cache-hit eligibility by generation** — generation 1's covered range starts
  at the live prefix (`covered_from` = the session's first entry id,
  `00000000`), so its summary request is cache-hit eligible; generation 2 starts
  after the generation-1 summary (`covered_from` = `00000004`), so it is
  cache-cold. Only the first compaction rides the warm cache.
- **Cache-write mass proxy** — the retained-tail tokens the next compaction
  rewrites to cache. In the microcompaction scenario this is 4520 estimated
  tokens (dominated by the large retained read), the mass a warm rewrite would
  re-bill.

What requires a live provider is **pending, not fabricated**: the summarization
request's cache-HIT rate (`cache_read_input_tokens / input_tokens`) and the
post-compaction cache-WRITE amplification in tokens
(`cache_write_input_tokens`). These come from `ProviderUsage` cache read/write
splits, which the fake-provider lane does not produce. They are documented
methodology with the measurement pending a recorded live lane; no number is
invented for them.

## Asserted minimum bars and contract tests

- `four_arms_each_clear_the_minimum_reduction_bar` — each of the four arms
  reduces its covered range by ≥ 60% (measured 77/77/77/71%).
- `seeded_arms_use_the_provider_summarizer_and_carry_paths` — both seeded arms
  invoke the provider summarizer (not the excerpts fallback), carry ≥ 1 path, and
  the microcompaction arm writes exactly one fold.
- `carry_path_is_retained_verbatim_in_rebuilt_context` — the carried path and the
  carry-block header survive verbatim on both seeded arms.
- `folded_detail_is_recoverable_behind_a_reference_not_retained` — the folded
  detail is absent from rebuilt context and the recall reference is present.
- `microcompaction_fold_stub_names_the_recoverable_path` — the fold stub names the
  recoverable path and the recall tool (fold-only view).
- `arms_separate_as_the_covered_range_grows` — the excerpts/provider ratio at a
  ten-turn covered range exceeds twice the single-message ratio.
- `generation_ordinal_advances_across_compactions` — at least two compactions
  fire, the generation ordinal is the 1-based compaction count, and generation 2
  covers a later range than generation 1.

## Measurement conditions

Debug build, fake-provider lane, fixed prompt/read sizes and fixed budgets, so
every covered range, summary, fold, generation, and ratio is reproducible in CI.
Tokens are `bench_support::est_tokens` estimates (4 bytes/token); no absolute
count is quoted as fact. This is measurement only — no production compaction,
fold, or recall behavior is changed.

## Out of scope (still open)

Not built here and deliberately left open (epic #379 known gaps and deferred
items): the over-budget-no-coverable-range floor, estimate-vs-actual token
calibration, provider-native context-management interplay, superseding/milestone
compaction, task-boundary triggers, and tiered routing.
