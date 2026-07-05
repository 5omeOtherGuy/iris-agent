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
on the shared measurement core in `src/tools/bench_support.rs`. Cache economics
are modeled deterministically here and anchored against realized `ProviderUsage`
by the env-gated live harness in `src/compaction_live_bench.rs` (never run under
the gate).

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

## Cache economics — modeled

The fake-provider lane reports no `ProviderUsage` cache splits, so cache mass is
**modeled**, never presented as provider-reported. The model: a prompt cache
serves the longest prefix a request shares with the previous request (cache-READ)
and re-bills the divergent suffix (cache-WRITE). Each compaction captures the
exact request payloads at the boundary (`CompactionFakeProvider` records every
`respond_stream` payload; `serialize_request` in `compaction_bench.rs`), and
`model_cache_economics` diffs the pre- and post-compaction payloads char-exact:
the common prefix is the modeled cache-READ mass, the divergent suffix is the
modeled cache-WRITE mass, both in `bench_support::est_tokens`. Every figure below
carries the label **modeled (prefix-divergence, estimated tokens)** — it is a
model, not a provider-reported split.

| arm | generation | pre req tok | post req tok | cache-READ (shared prefix) | cache-WRITE (divergent suffix) |
|---|---|---|---|---|---|
| provider | 1 | 405 | 127 | 2 | 126 |
| provider+carry | 1 | 4912 | 4536 | 2 | 4534 |
| provider+carry+microcompaction | 1 | 4912 | 4526 | 2 | 4525 |
| provider (multi-generation) | 1 | 378 | 405 | 2 | 404 |
| provider (multi-generation) | 2 | 405 | 435 | 84 | 352 |
| provider (multi-generation) | 3 | 435 | 518 | 165 | 353 |
| provider (multi-generation) | 4 | 518 | 600 | 247 | 353 |

Reading the model:

- **Generation 1 collapses the cache.** The covered range starts at the live
  prefix (`covered_from` = the session's first entry id, `00000000`), so the
  summary rewrites that prefix: the modeled cache-READ is ~0 and the
  post-compaction cache-WRITE is essentially the whole request. Asserted per
  provider arm (`modeled_cache_write_dominates_the_post_compaction_request`):
  cache-WRITE ≥ 50 tokens and ≥ half the post request.
- **Later generations warm.** After generation 1 a stable summary + tail prefix
  persists, so the modeled cache-READ accrues generation over generation
  (2 → 84 → 165 → 247) while cache-WRITE stays bounded (~350). Asserted by
  `modeled_cache_read_warms_across_generations` (READ strictly grows across
  generations; generation 2's `covered_from` = `00000004` is a later range).
- The deterministic **excerpts** arm makes no provider summarization call, so it
  issues no summary request and has no provider-side summary-request cache
  economics to model; its compaction is provider-invisible.

Structural cross-check (kept from the seam, not the model): the post-compaction
retained-tail estimate is 4520 tokens in the microcompaction scenario — a coarse
proxy for the `keep_target` rewrite mass, consistent with the modeled cache-WRITE
column.

## Cache economics — live validation (Anthropic Claude Code OAuth)

The modeled metric is anchored against realized `ProviderUsage` from the
Anthropic Messages provider on the Claude Code subscription OAuth lane — the only
lane that reports both cache reads AND writes plus the 5m/1h tier split. The
harness (`src/compaction_live_bench.rs`) is double-gated (`#[ignore]` +
`IRIS_BENCH_LIVE=1`) so `scripts/gate.sh` never issues a live call; a test-only
`RecordingProvider` wrapper captures the usage on every completed turn (the
production `provider_summary` path discards it). It seeds a near-budget session,
resumes on the OAuth lane, warms the cache on turn 1, drives a compaction on
turn 2, and captures the summarization request and the first post-compaction
request.

**Run SUCCEEDED.** Lane: Anthropic Messages / Claude Code OAuth. Model:
`claude-sonnet-4-6`. Capture date: 2026-07-05.

| request | input_tokens | cache_read | cache_write | 5m / 1h tier |
|---|---|---|---|---|
| summarization | 1463 | 0 | 1460 | — |
| first post-compaction | 2599 | 0 | 2596 | 2596 / 0 |

Realized vs. modeled:

- **Cache-WRITE validated.** The post-compaction request writes 2596 of 2599
  input tokens (99.9%) — the realized counterpart of the model's post-compaction
  cache-WRITE dominance. The write lands entirely in the 5-minute tier
  (2596 / 0), the split only Anthropic reports.
- **Cache-HIT realized 0.00, honestly.** Both requests show `cache_read = 0`.
  Realizing a warm summarization hit needs a breakpoint-aligned prefix re-sent
  across turns; a three-turn synthetic resume does not reproduce that alignment,
  so the covered range is written fresh (`cache_write = 1460` on the summary
  request) rather than read warm. The modeled warm-cache premise (ADR-0041/0045)
  therefore remains a model for the multi-turn steady state, anchored here only
  on the write side. No hit-rate number is invented.

The exact captured output (unix date `1783251191`):

```
summarization request: input_tokens=1463, cache_read=0, cache_write=1460, realized cache-HIT rate=0.00
first post-compaction request: input_tokens=2599, cache_read=0, cache_write=2596 (5m=2596, 1h=0)
```

## Provider asymmetry

The two lanes report cache accounting asymmetrically, so the cache-WRITE column
means different things per provider:

- **Anthropic Messages (Claude Code OAuth)** reports `cache_read_input_tokens`
  AND `cache_write_input_tokens`, plus the `cache_creation` 5m/1h tier split. Both
  the modeled cache-READ and cache-WRITE masses have a directly realized
  counterpart (validated above).
- **Codex Responses** reports `cache_read_input_tokens` only;
  `cache_write_input_tokens` is hardcoded `0` and `cache_creation` is `None`
  (`src/mimir/providers/openai_codex_responses.rs:854`) — a provider limitation,
  not a missing measurement (OpenAI reports no write metric). Its cache column
  carries the hit rate PLUS a **derived** fresh-input amplification:
  `input_tokens - cached_tokens` on the first post-compaction request vs. the
  pre-compaction baseline. The modeled divergent-suffix mass is the deterministic
  stand-in for that derived amplification.

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
- `modeled_cache_write_dominates_the_post_compaction_request` — per provider arm,
  the modeled cache-WRITE (divergent suffix) is ≥ 50 tokens and ≥ half the
  post-compaction request, and READ + WRITE reconstruct it (± 1 for rounding).
- `modeled_cache_read_warms_across_generations` — across ≥ 2 modeled generations,
  the cache-READ (shared prefix) strictly grows as a stable summary + tail prefix
  persists.

## Measurement conditions

Debug build, fake-provider lane, fixed prompt/read sizes and fixed budgets, so
every covered range, summary, fold, generation, and ratio is reproducible in CI.
Tokens are `bench_support::est_tokens` estimates (4 bytes/token); no absolute
count is quoted as fact. This is measurement only — no production compaction,
fold, or recall behavior is changed. The live anchor lane is env-gated
(`IRIS_BENCH_LIVE=1` + `#[ignore]`) and excluded from the gate/CI; its numbers
are captured on the Anthropic Claude Code OAuth lane with the model id and
capture date recorded beside them.

## Out of scope (still open)

Not built here and deliberately left open (epic #379 known gaps and deferred
items): the over-budget-no-coverable-range floor, estimate-vs-actual token
calibration, provider-native context-management interplay, superseding/milestone
compaction, task-boundary triggers, and tiered routing.
