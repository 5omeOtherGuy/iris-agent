# Issue #400 fold-flush cost — what a fold-only prefix break pays

What microcompaction actually costs. The slice-B benchmark
(`issue-372-compaction-retention-slice-b.md`) measured folding only at a
compaction boundary, where the compaction masks the fold's own cache break.
This benchmark isolates the fold: a budget high enough that compaction never
fires, so the micro-watermark flush is the only transcript rewrite, and its
price and payback are measured alone. Modeled figures are deterministic and
CI-asserted (`src/compaction_bench.rs`, fold-flush section); realized figures
are one env-gated live capture (`src/compaction_live_bench.rs`).

Regenerate the modeled tables:

```
cargo test fold_flush_cost_benchmark_report -- --nocapture
```

Regenerate the live capture (real API calls, ~4 requests):

```
IRIS_BENCH_LIVE=1 cargo test fold_flush_cost_live_anthropic -- --ignored --nocapture
```

## Fold-only flush — modeled (prefix-divergence, estimated tokens)

Two byte-identical runs of the carry seed (superseded ~220-token read early,
~4400-token superseding read later) under a compaction-proof budget; the only
difference is `microcompaction` on/off. Turn 1 carries the flush boundary;
turn 2 is steady state.

| run | turn-1 cache-READ | turn-1 cache-WRITE | turn-2 request tok | folds |
|---|---|---|---|---|
| control (micro off) | 4912 | 14 | 4940 | 0 |
| fold-only arm (micro on) | 283 | 4499 | 4796 | 1 |

- **Marginal flush cost: 4485 modeled tokens** (asserted `>= 1000`,
  `modeled_marginal_cost_of_a_fold_only_flush`). The flush breaks the prefix at
  the folded read; everything after it re-bills. The control's divergence is
  the appended prompt alone (asserted `< 100`).
- **Steady-state saving: 144 modeled tokens per subsequent request** (asserted
  `>= 100`, `fold_only_flush_shrinks_every_subsequent_request`); the stub and
  its recovery affordance survive verbatim (needles `[folded]`, the path).
- **The break is one-time**: post-flush turns are append-only again (asserted
  `< 100` divergence between arm turns 1 and 2).
- **Warm-cache break-even: ~359 turns** under stated Anthropic 5m pricing
  ratios (write 1.25x, read 0.10x base input — published-pricing assumptions,
  not measurements). One-time extra `4485 x (1.25 - 0.10)` vs per-turn saving
  `144 x 0.10`.

## Same-boundary flush — the piggyback case (#400 trigger 1)

| arm | generation-1 post req tok | cache-WRITE (divergent suffix) |
|---|---|---|
| provider+carry (no folds) | 4536 | 4534 |
| provider+carry+microcompaction | 4526 | 4525 |

When the flush lands on the same boundary as a compaction, the compaction
rewrites the prefix anyway: the fold's marginal cache-WRITE is zero or negative
(asserted `same_boundary_fold_flush_adds_no_marginal_write`).

## Fold-only flush — realized (Anthropic Claude Code OAuth)

One live capture; lane: Anthropic Messages / Claude Code OAuth; model:
`claude-sonnet-4-6`; unix_date `1783264117`. Seed ~2828 estimated tokens with a
paired tool_use/tool_result superseded read; two runs (micro off/on), two
normal turns each; the flush lands at the turn-2 boundary so turn 1 warms the
original prefix.

| run, turn-2 request | input_tokens | cache_read | cache_write | 5m / 1h |
|---|---|---|---|---|
| control (no fold) | 2534 | 2488 | 43 | 43 / 0 |
| arm (post-flush) | 2175 | 0 | 2172 | 2172 / 0 |

- **Realized marginal fold cost: 2129 provider-reported write tokens**
  (`2172 - 43`); the realized read drop is 2488 tokens (the fold point is early
  in the transcript, so the entire prefix re-bills).
- **Realized residency saving: 359 tokens per request** (`2534 - 2175`).
- **Realized warm-cache break-even on this seed: ~68 turns**
  (`2129 x 1.15 / (359 x 0.10)`, same stated pricing ratios).
- **Normal-turn requests realize cache hits.** The control's turn-2 read 2488
  of 2534 input tokens from cache — unlike slice-B's live capture, where the
  summarization request realized 0. Two variables differ (this seed is larger,
  ~2828 vs ~963 estimated tokens, and both requests here are normal turn
  requests with identical shape); this capture cannot attribute the fix between
  them, but it establishes that the warm-read side of the model is realizable
  on this lane, not just the write side.

## Reading the numbers

- A fold-only flush on a **warm** cache is expensive: it re-bills everything
  below the fold point (modeled 4485; realized 2129) to save the folded body
  per turn (modeled 144; realized 359). Break-even is tens to hundreds of
  turns, depending on where the fold sits and how big the transcript is.
- The same fold at a **compaction boundary is free** (modeled marginal
  `<= 0`) — the compaction pays for the rewrite regardless.
- On a **cold** cache (idle past TTL, cold resume) the suffix re-bills
  regardless, so the flush is free and the saving is immediate.
- Together these are the economics behind #400's trigger list: flush folds
  when a prefix break is inevitable anyway (compaction, prompt/model change,
  inferred TTL expiry), keep the watermark as backstop.

## Measurement conditions

Modeled lane: debug build, fake provider, fixed seeds/prompts/budgets;
`bench_support::est_tokens` (4 bytes/token) over the canonical request
serialization; ratios meaningful, absolute counts not. All modeled figures are
labeled modeled (prefix-divergence, estimated tokens) and asserted as minimum
bars, never exact values. Live lane: double-gated (`#[ignore]` +
`IRIS_BENCH_LIVE=1`), never run by `scripts/gate.sh` or CI; numbers are
provider-reported `ProviderUsage` splits from single requests (no averaging);
pricing ratios in the break-even lines are stated assumptions from published
Anthropic 5m-tier pricing. No production compaction, fold, or recall behavior
is changed by this benchmark.
